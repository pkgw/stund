// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! Interfacing with the daemon.

use atty;
use failure::Error;
use futures::{Async, Future, Poll, Sink, Stream};
use futures::future::Either;
use futures::sink::Send;
use futures::stream::StreamFuture;
use futures::sync::mpsc::Receiver;
use libc;
use state_machine_future::RentToOwn;
use std::env;
use std::io::{self, Write};
use std::mem;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use tokio_core::reactor::Core;
use tokio_executor;
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_io::codec::length_delimited::{FramedRead, FramedWrite};
use tokio_io::io::{ReadHalf, WriteHalf};
use tokio_serde_json::{ReadJson, WriteJson};
use tokio_stdin;
use tokio_uds::UnixStream;

use stund::*;
use stund::protocol::*;
use super::StundOpenOptions;

fn get_socket_path() -> Result<PathBuf, Error> {
    let mut p = env::home_dir().ok_or(format_err!("unable to determine your home directory"))?;
    p.push(".ssh");
    p.push("stund.sock");
    Ok(p)
}

type Ser = WriteJson<FramedWrite<WriteHalf<UnixStream>>, ClientMessage>;
type De = ReadJson<FramedRead<ReadHalf<UnixStream>>, ServerMessage>;

#[derive(StateMachineFuture)]
#[allow(unused)] // get lots of these spuriously; custom derive stuff?
enum OpenWorkflow {
    #[state_machine_future(start, transitions(FirstAck))]
    Issue {
        opts: StundOpenOptions,
        de: De,
        transmission: Send<Ser>,
    },

    #[state_machine_future(transitions(FirstAck, Communicating))]
    FirstAck {
        ser: Ser,
        reception: StreamFuture<De>,
    },

    #[state_machine_future(transitions(Communicating, CleaningUpIo))]
    Communicating {
        stdin: StreamFuture<Receiver<u8>>,
        buf: Vec<u8>,
        finished: FinishCommunicationState,
        stdout: io::Stdout,
        transmission: Either<Ser, Send<Ser>>,
        reception: StreamFuture<De>,
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


pub fn new_open(opts: StundOpenOptions) -> Result<(), Error> {
    let mut core = Core::new()?;
    let handle = core.handle();
    let conn = UnixStream::connect(get_socket_path()?, &handle)?;

    unsafe {
        // Without turning on linger, I find that the tokio-ized version loses
        // the last bytes of the session. Let's just ignore the return value
        // of setsockopt(), though.
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
    let fut = ser.send(ClientMessage::Open(OpenParameters { host: opts.host.clone() }));
    let result = core.run(OpenWorkflow::start(opts, de, fut));
    toggle_terminal_echo(true);
    let (ser, _de) = result?;
    println!("[Success]");

    core.run(ser.send(ClientMessage::Goodbye))?;

    Ok(())
}

impl PollOpenWorkflow for OpenWorkflow {
    fn poll_issue<'a>(
        state: &'a mut RentToOwn<'a, Issue>
    ) -> Poll<AfterIssue, Error> {
        let ser = try_ready!(state.transmission.poll());

        let state = state.take();
        transition!(FirstAck {
            ser: ser,
            reception: state.de.into_future(),
        })
    }

    fn poll_first_ack<'a>(
        state: &'a mut RentToOwn<'a, FirstAck>
    ) -> Poll<AfterFirstAck, Error> {
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
                let mut state = state.take();
                state.reception = de.into_future();
                transition!(state);
            },
        }

        println!("[Begin SSH login; type \".\" on an empty line when login has succeeded.]");
        toggle_terminal_echo(false);

        let stdin = tokio_stdin::spawn_stdin_stream_bounded(512);

        let state = state.take();
        transition!(Communicating {
            stdin: stdin.into_future(),
            buf: Vec::new(),
            finished: FinishCommunicationState::SawFirstEnter,
            stdout: io::stdout(),
            transmission: Either::A(state.ser),
            reception: de.into_future(),
        })
    }

    fn poll_communicating<'a>(
        state: &'a mut RentToOwn<'a, Communicating>
    ) -> Poll<AfterCommunicating, Error> {
        // New text from the daemon?

        let de = {
            let outcome = match state.reception.poll() {
                Ok(x) => x,
                Err((e, _de)) => {
                    return Err(e.into());
                },
            };

            if let Async::Ready((msg, de)) = outcome {
                match msg {
                    Some(ServerMessage::SshData(data)) => {
                        state.stdout.write_all(&data)?;
                        state.stdout.flush()?;
                    },

                    Some(ServerMessage::Error(e)) => {
                        println!("");
                        return Err(format_err!("{}", e));
                    }

                    Some(other) => {
                        println!("");
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

        let (stdin, finished) = {
            let outcome = match state.stdin.poll() {
                Ok(x) => x,
                Err((_, _stdin)) => {
                    return Err(format_err!("error reading from the terminal"));
                },
            };

            if let Async::Ready((byte, stdin)) = outcome {
                let finished = match byte {
                    Some(b) => {
                        state.buf.push(b);
                        state.finished.transition(b)
                    },

                    None => {
                        state.finished
                    },
                };

                (Some(stdin), finished)
            } else {
                (None, state.finished)
            }
        };

        // Ready/able to send bytes to the daemon?

        let mut state = state.take();

        let transmission = match state.transmission {
            Either::A(ser) => {
                if state.buf.len() != 0 {
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

        // Finally ready to figure out what our next step is. It's a bit of a
        // hassle to make sure that we clean up any pending operations
        // gracefully.

        if let FinishCommunicationState::SawSecondEnter = finished {
            // If we're here, we must have just gotten a byte off of stdin,
            // so we know that the underlying stream has been extracted.
            stdin.unwrap().close();

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
            state.transmission = transmission;
            state.finished = finished;

            if let Some(de) = de {
                state.reception = de.into_future();
            }

            if let Some(stdin) = stdin {
                state.stdin = stdin.into_future();
            }

            transition!(state)
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
                    Some(ServerMessage::SshData(data)) => {
                        let mut stdout = io::stdout();
                        stdout.write_all(&data)?;
                        stdout.flush()?;
                    },

                    Some(ServerMessage::Error(e)) => {
                        println!("");
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


fn toggle_terminal_echo(active: bool) {
    if atty::isnt(atty::Stream::Stdout) {
        return;
    }

    let mut attrs: libc::termios = unsafe { mem::zeroed() };

    if unsafe { libc::tcgetattr(0, &mut attrs as _) } != 0 {
        println!("error querying terminal attributes?!");
        return;
    }

    if active {
        attrs.c_lflag |= libc::ECHO;
    } else {
        attrs.c_lflag &= !libc::ECHO;
    }

    if unsafe { libc::tcsetattr(0, libc::TCSANOW, &attrs as _) } != 0 {
        println!("error setting terminal attributes?!");
    }
}
