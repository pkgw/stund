// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! Interfacing with the daemon.

use failure::Error;
use futures::{Async, Future, Poll, Sink, Stream};
use futures::future::Either;
use futures::sink::Send;
use futures::stream::StreamFuture;
use libc;
use state_machine_future::RentToOwn;
use std::io;
use std::mem;
use std::os::unix::io::AsRawFd;
use tokio_core::reactor::{Core, Handle};
use tokio_io::AsyncRead;
use tokio_io::codec::length_delimited::{FramedRead, FramedWrite};
use tokio_io::io::{ReadHalf, WriteHalf};
use tokio_serde_json::{ReadJson, WriteJson};
use tokio_uds::UnixStream;

use super::*;


type Ser = WriteJson<FramedWrite<WriteHalf<UnixStream>>, ClientMessage>;
type De = ReadJson<FramedRead<ReadHalf<UnixStream>>, ServerMessage>;

pub struct Connection {
    core: Core,
    ser: Ser,
    de: De,
}

impl Connection {
    pub fn establish() -> Result<Self, Error> {
        let core = Core::new()?;
        let handle = core.handle();

        // TODO: launch daemon if can't connect and some `autolaunch` option
        // is true.
        let conn = UnixStream::connect(get_socket_path()?, &handle)?;

        unsafe {
            // Without turning on linger, I find that the tokio-ized version
            // loses the last bytes of the session. Let's just ignore the
            // return value of setsockopt(), though.
            let linger = libc::linger { l_onoff: 1, l_linger: 2 };
            libc::setsockopt(conn.as_raw_fd(), libc::SOL_SOCKET, libc::SO_LINGER,
                             (&linger as *const libc::linger) as _,
                             mem::size_of::<libc::linger>() as libc::socklen_t);
        }

        let (read, write) = conn.split();
        let wdelim = FramedWrite::new(write);
        let ser = WriteJson::new(wdelim);
        let rdelim = FramedRead::new(read);
        let de = ReadJson::new(rdelim);

        Ok(Connection {
            core: core,
            ser: ser,
            de: de,
        })
    }


    pub fn handle(&self) -> Handle {
        self.core.handle()
    }


    pub fn close(mut self) -> Result<(), Error> {
        self.core.run(self.ser.send(ClientMessage::Goodbye))?;
        Ok(())
    }


    pub fn send_open<T, R>(
        mut self, params: OpenParameters, tx_user: T, rx_user: R
    ) -> Result<Self, Error>
        where T: 'static + Sink<SinkItem = Vec<u8>, SinkError = io::Error>,
              R: 'static + Stream<Item = Vec<u8>, Error = io::Error>
    {
        let fut = self.ser.send(ClientMessage::Open(params));
        let (ser, de) = self.core.run(OpenWorkflow::start(fut, self.de, Box::new(tx_user), Box::new(rx_user)))?;
        self.ser = ser;
        self.de = de;
        Ok(self)
    }
}


pub trait OpenInteraction {
    fn get_handles(&self) -> Result<(UserOutputSink, UserInputStream), Error>;
}


#[derive(StateMachineFuture)]
#[allow(unused)] // get lots of these spuriously; custom derive stuff?
enum OpenWorkflow {
    #[state_machine_future(start, transitions(FirstAck))]
    Issue {
        tx_ssh: Send<Ser>,
        rx_ssh: De,
        tx_user: UserOutputSink,
        rx_user: UserInputStream,
    },

    #[state_machine_future(transitions(FirstAck, Communicating))]
    FirstAck {
        ser: Ser,
        reception: StreamFuture<De>,
        tx_user: UserOutputSink,
        rx_user: UserInputStream,
    },

    #[state_machine_future(transitions(Communicating, CleaningUpIo))]
    Communicating {
        rx_user: StreamFuture<UserInputStream>,
        tx_user: Either<UserOutputSink, Send<UserOutputSink>>,
        user_buf: Vec<u8>,
        finished: FinishCommunicationState,
        transmission: Either<Ser, Send<Ser>>,
        reception: StreamFuture<De>,
        buf: Vec<u8>,
    },

    #[state_machine_future(transitions(CleaningUpIo, Finished))]
    CleaningUpIo {
        transmission: Either<Ser, Send<Ser>>,
        reception: StreamFuture<De>,
        sent_finished_message: bool,
    },

    #[state_machine_future(ready)]
    Finished((Ser, De)),

    #[state_machine_future(error)]
    Failed(Error),
}

type UserInputStream = Box<Stream<Item = Vec<u8>, Error = io::Error>>;
type UserOutputSink = Box<Sink<SinkItem = Vec<u8>, SinkError = io::Error>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FinishCommunicationState {
    NoLeads,
    SawFirstEnter,
    SawPeriod,
    SawSecondEnter,
}

impl FinishCommunicationState {
    pub fn transition(&self, byte: u8) -> Self {
        match *self {
            FinishCommunicationState::NoLeads => {
                if byte == 0x0A {
                    return FinishCommunicationState::SawFirstEnter;
                }
            },

            FinishCommunicationState::SawFirstEnter => {
                if byte == 0x2E {
                    return FinishCommunicationState::SawPeriod;
                }
            },

            FinishCommunicationState::SawPeriod => {
                if byte == 0x0A {
                    return FinishCommunicationState::SawSecondEnter;
                }
            },

            FinishCommunicationState::SawSecondEnter => {
                return FinishCommunicationState::SawSecondEnter;
            },
        }

        FinishCommunicationState::NoLeads
    }
}


impl PollOpenWorkflow for OpenWorkflow {
    fn poll_issue<'a>(
        state: &'a mut RentToOwn<'a, Issue>
    ) -> Poll<AfterIssue, Error> {
        eprintln!("poll issue");
        let ser = try_ready!(state.tx_ssh.poll());

        let state = state.take();
        transition!(FirstAck {
            ser: ser,
            reception: state.rx_ssh.into_future(),
            tx_user: state.tx_user,
            rx_user: state.rx_user,
        })
    }

    fn poll_first_ack<'a>(
        state: &'a mut RentToOwn<'a, FirstAck>
    ) -> Poll<AfterFirstAck, Error> {
        eprintln!("poll first");
        let (msg, de) = match state.reception.poll() {
            Ok(Async::Ready((msg, de))) => (msg, de),
            Ok(Async::NotReady) => {
                return Ok(Async::NotReady);
            },
            Err((e, _de)) => {
                return Err(e.into());
            }
        };

        match msg {
            Some(ServerMessage::Ok) => {},

            Some(ServerMessage::Error(text)) => {
                return Err(format_err!("{}", text));
            },

            Some(other) => {
                return Err(format_err!("unexpected response from daemon: {:?}", other));
            },

            None => {
                return Err(format_err!("connection closed (?)"));
            },
        }

        let state = state.take();

        transition!(Communicating {
            rx_user: state.rx_user.into_future(),
            tx_user: Either::A(state.tx_user),
            user_buf: Vec::new(),
            finished: FinishCommunicationState::SawFirstEnter,
            transmission: Either::A(state.ser),
            reception: de.into_future(),
            buf: Vec::new(),
        })
    }

    fn poll_communicating<'a>(
        state: &'a mut RentToOwn<'a, Communicating>
    ) -> Poll<AfterCommunicating, Error> {
        eprintln!("communicate");

        // New text from the daemon?

        let de = {
            let outcome = match state.reception.poll() {
                Ok(x) => x,
                Err((e, _de)) => {
                    eprintln!("e1");
                    return Err(e.into());
                },
            };

            eprintln!("something from SSH");

            if let Async::Ready((msg, de)) = outcome {
                match msg {
                    Some(ServerMessage::SshData(data)) => {
                        eprintln!("ssh data");
                        state.user_buf.extend_from_slice(&data);
                    },

                    Some(ServerMessage::Error(e)) => {
                        //println!("");
                        eprintln!("e2");
                        return Err(format_err!("{}", e));
                    }

                    Some(other) => {
                        //println!("");
                        eprintln!("e3");
                        return Err(format_err!("unexpected message from the daemon: {:?}", other));
                    },

                    None => {},
                }

                Some(de)
            } else {
                None
            }
        };

        // New text from the user?

        let (rx_user, finished) = {
            let outcome = match state.rx_user.poll() {
                Ok(x) => x,
                Err((_, _rx_user)) => {
                    eprintln!("e4");
                    return Err(format_err!("error reading from the terminal"));
                },
            };

            if let Async::Ready((bytes, rx_user)) = outcome {
                let finished = match bytes {
                    None => {
                        return Err(format_err!("EOF on terminal (?)"));
                    },

                    Some(b) => {
                        eprintln!("user data");
                        state.buf.extend_from_slice(&b);

                        let mut t = state.finished;

                        for single_byte in &b {
                            t = t.transition(*single_byte);
                        }

                        t
                    },

                };

                (Some(rx_user), finished)
            } else {
                (None, state.finished)
            }
        };

        // Ready/able to send bytes to the daemon?

        let mut state = state.take();

        let transmission = match state.transmission {
            Either::A(ser) => {
                if state.buf.len() != 0 {
                    eprintln!("daemon tx");
                    let send = ser.send(ClientMessage::UserData(state.buf.clone()));
                    state.buf.clear();
                    Either::B(send)
                } else {
                    Either::A(ser)
                }
            },

            Either::B(mut send) => {
                Either::A(try_ready!(send.poll()))
            },
        };

        // Ready/able to send bytes to the user?

        let tx_user = match state.tx_user {
            Either::A(ser) => {
                if state.user_buf.len() != 0 {
                    eprintln!("user tx");
                    let send = ser.send(state.user_buf.clone().into());
                    state.user_buf.clear();
                    Either::B(send)
                } else {
                    Either::A(ser)
                }
            },

            Either::B(mut send) => {
                Either::A(try_ready!(send.poll()))
            },
        };

        // Finally ready to figure out what our next step is. It's a bit of a
        // hassle to make sure that we clean up any pending operations
        // gracefully.

        if let FinishCommunicationState::SawSecondEnter = finished {
            eprintln!("finish??");

            let (transmission, sent_finished_message) = match transmission {
                Either::A(ser) => {
                    (Either::B(ser.send(ClientMessage::EndOfUserData)), true)
                },

                other => {
                    (other, false)
                },
            };

            let reception = if let Some(de) = de {
                de.into_future()
            } else {
                state.reception
            };

            transition!(CleaningUpIo {
                transmission: transmission,
                reception: reception,
                sent_finished_message: sent_finished_message
            })
        } else {
            eprintln!("loop");
            state.transmission = transmission;
            state.finished = finished;
            state.tx_user = tx_user;

            if let Some(de) = de {
                state.reception = de.into_future();
            }

            if let Some(rx_user) = rx_user {
                state.rx_user = rx_user.into_future();
            }

            transition!(state);
        }
    }

    fn poll_cleaning_up_io<'a>(
        state: &'a mut RentToOwn<'a, CleaningUpIo>
    ) -> Poll<AfterCleaningUpIo, Error> {
        // Ready/able/needed to send end-of-data message?

        let mut state = state.take();

        let (transmission, msg_flushed) = match state.transmission {
            Either::A(ser) => {
                if !state.sent_finished_message {
                    state.sent_finished_message = true;
                    (Either::B(ser.send(ClientMessage::EndOfUserData)), false)
                } else {
                    (Either::A(ser), true)
                }
            },

            Either::B(mut send) => {
                (Either::A(try_ready!(send.poll())), state.sent_finished_message)
            },
        };

        // New stuff from the daemon?

        let (de, got_ok) = {
            let outcome = match state.reception.poll() {
                Ok(x) => x,
                Err((e, _de)) => {
                    return Err(e.into());
                },
            };

            if let Async::Ready((msg, de)) = outcome {
                let mut got_ok = false;

                match msg {
                    // Might as well print this out
                    Some(ServerMessage::SshData(_data)) => {
                        //println!("blah blah ignoring trailing data");
                    },

                    Some(ServerMessage::Error(e)) => {
                        //println!("");
                        return Err(format_err!("{}", e));
                    }

                    Some(ServerMessage::Ok) => {
                        got_ok = true;
                    }

                    //Some(other) => {
                    //    println!("");
                    //    return Err(format_err!("unexpected message from the daemon: {:?}", other));
                    //},

                    None => {},
                }

                (Some(de), got_ok)
            } else {
                (None, false)
            }
        };

        // What's next?

        if msg_flushed && got_ok {
            if let Either::A(ser) = transmission {
                transition!(Finished((ser, de.unwrap())))
            } else {
                panic!("internal logic failure");
            }
        } else {
            state.transmission = transmission;
            transition!(state)
        }
    }
}
