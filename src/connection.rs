use std::io::{Error,ErrorKind,Read,Result,Write};
use std::str;
use std::iter::repeat;
use std::collections::HashMap;
use nom::{HexDisplay,IResult,Offset};
use sasl::{SaslCredentials, SaslSecret, SaslMechanism};
use sasl::mechanisms::Plain;

use format::frame::*;
use format::field::*;
use channel::{Channel,ChannelState};
use buffer::Buffer;
use generated::*;

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum ConnectionState {
  Initial,
  Connecting(ConnectingState),
  Connected,
  Closing(ClosingState),
  Closed,
  Error,
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum ConnectingState {
  Initial,
  SentProtocolHeader,
  ReceivedStart,
  SentStartOk,
  ReceivedSecure,
  SentSecure,
  ReceivedSecondSecure,
  ReceivedTune,
  SentOpen,
  ReceivedOpenOk,
  Error,
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum ClosingState {
  Initial,
  SentClose,
  ReceivedClose,
  SentCloseOk,
  ReceivedCloseOk,
  Error,
}

#[derive(Clone,Debug,PartialEq)]
pub struct Connection {
  pub state:            ConnectionState,
  pub channels:         HashMap<u16, Channel>,
  pub send_buffer:      Buffer,
  pub receive_buffer:   Buffer,
}

impl Connection {
  pub fn new() -> Connection {
    let mut h = HashMap::new();
    h.insert(0, Channel::global());

    Connection {
      state:    ConnectionState::Initial,
      channels: h,
      send_buffer:    Buffer::with_capacity(8192),
      receive_buffer: Buffer::with_capacity(8192),
    }
  }

  pub fn connect(&mut self, writer: &mut Write) -> Result<ConnectionState> {
    if self.state != ConnectionState::Initial {
      self.state = ConnectionState::Error;
      return Err(Error::new(ErrorKind::Other, "invalid state"))
    }

    let res = gen_protocol_header((self.send_buffer.space(), 0)).map(|tup| tup.1);
    if let Ok(sz) = res {
      self.send_buffer.fill(sz);
      match writer.write(&mut self.send_buffer.data()) {
        Ok(sz2) => {
          self.send_buffer.consume(sz2);
          self.state = ConnectionState::Connecting(ConnectingState::SentProtocolHeader);
          Ok(self.state)
        },
        Err(e) => Err(e),
      }
    } else {
      Err(Error::new(ErrorKind::WouldBlock, "could not write protocol header"))
    }
  }

  pub fn write(&mut self, writer: &mut Write) -> Result<ConnectionState> {
    println!("will write:\n{}", (&self.send_buffer.data()).to_hex(16));
    match writer.write(&mut self.send_buffer.data()) {
      Ok(sz) => {
        println!("wrote {} bytes", sz);
        self.send_buffer.consume(sz);
        Ok(self.state)
      },
      Err(e) => Err(e),
    }
  }

  pub fn read(&mut self, reader: &mut Read) -> Result<ConnectionState> {
    if self.state == ConnectionState::Initial || self.state == ConnectionState::Error {
      self.state = ConnectionState::Error;
      return Err(Error::new(ErrorKind::Other, "invalid state"))
    }

    match reader.read(&mut self.receive_buffer.space()) {
      Ok(sz) => {
        println!("read {} bytes", sz);
        self.receive_buffer.fill(sz);
      },
      Err(e) => return Err(e),
    }
    println!("will parse:\n{}", (&self.receive_buffer.data()).to_hex(16));
    let (channel_id, method, consumed) = {
      let parsed_frame = raw_frame(&self.receive_buffer.data());
      match parsed_frame {
        IResult::Done(i,_)     => {},
        IResult::Incomplete(_) => {
          return Ok(self.state);
        },
        IResult::Error(e) => {
          //FIXME: should probably disconnect on error here
          let err = format!("parse error: {:?}", e);
          self.state = ConnectionState::Error;
          return Err(Error::new(ErrorKind::Other, err))
        }
      }

      let (i, f) = parsed_frame.unwrap();

      //println!("parsed frame: {:?}", f);
      //FIXME: what happens if we fail to parse a packet in a channel?
      // do we continue?
      let consumed = self.receive_buffer.data().offset(i);

      match f.frame_type {
        FrameType::Method => {
          let parsed = parse_class(f.payload);
          println!("parsed method: {:?}", parsed);
          match parsed {
            IResult::Done(b"", m) => {
              (f.channel_id, m, consumed)
            },
            e => {
              //we should not get an incomplete here
              //FIXME: should probably disconnect channel on error here
              let err = format!("parse error: {:?}", e);
              if f.channel_id == 0 {
                self.state = ConnectionState::Error;
              } else {
                self.channels.get_mut(&f.channel_id).map(|channel| channel.state = ChannelState::Error);
              }
              return Err(Error::new(ErrorKind::Other, err))
            }
          }
        },
        t => {
          println!("frame type: {:?} -> unknown payload:\n{}", t, f.payload.to_hex(16));
          let err = format!("parse error: {:?}", t);
          return Err(Error::new(ErrorKind::Other, err))
        }
      }
    };

    self.receive_buffer.consume(consumed);

    if channel_id == 0 {
      self.handle_global_method(method);
    } else {
      self.channels.get_mut(&channel_id).map(|channel| channel.received_method(method));
    }


    return Ok(ConnectionState::Connected);


    unreachable!();
  }

  pub fn handle_global_method(&mut self, c: Class) {
    match self.state {
      ConnectionState::Initial | ConnectionState::Closed | ConnectionState::Error => {
        self.state = ConnectionState::Error
      },
      ConnectionState::Connecting(connecting_state) => {
        match connecting_state {
          ConnectingState::Initial | ConnectingState::Error => {
            self.state = ConnectionState::Error
          },
          ConnectingState::SentProtocolHeader => {
            if let Class::Connection(connection::Methods::Start(s)) = c {
              println!("Server sent Connection::Start: {:?}", s);
              self.state = ConnectionState::Connecting(ConnectingState::ReceivedStart);

              let mut h = HashMap::new();
              h.insert("product".to_string(), Value::LongString("lapin".to_string()));

              let creds = SaslCredentials {
                username: "guest".to_owned(),
                secret: SaslSecret::Password("guest".to_owned()),
                channel_binding: None,
              };

              let mut mechanism = Plain::from_credentials(creds).unwrap();

              let initial_data = mechanism.initial().unwrap();
              let s = str::from_utf8(&initial_data).unwrap();

              //FIXME: fill with user configured data
              let start_ok = Class::Connection(connection::Methods::StartOk(
                connection::StartOk {
                  client_properties: h,
                  mechanism: "PLAIN".to_string(),
                  locale:    "en_US".to_string(), // FIXME: comes from the server
                  response:  s.to_string(),     //FIXME: implement SASL
                }
              ));

              println!("client sending Connection::StartOk: {:?}", start_ok);

              match gen_method_frame((&mut self.send_buffer.space(), 0), 0, &start_ok).map(|tup| tup.1) {
                Ok(sz) => {
                  self.send_buffer.fill(sz);
                  self.state = ConnectionState::Connecting(ConnectingState::SentStartOk);
                }
                Err(e) => {
                  println!("error generating start-ok frame: {:?}", e);
                  self.state = ConnectionState::Error;
                },
              }
            } else {
              println!("waiting for class Connection method Start, got {:?}", c);
              self.state = ConnectionState::Error;
            }
          },
          ConnectingState::ReceivedStart => {},
          ConnectingState::SentStartOk => {},
          ConnectingState::ReceivedSecure => {},
          ConnectingState::SentSecure => {},
          ConnectingState::ReceivedSecondSecure => {},
          ConnectingState::ReceivedTune => {},
          ConnectingState::SentOpen => {},
          ConnectingState::ReceivedOpenOk => {},
          ConnectingState::Error => {},
        }
      },
      ConnectionState::Connected => {},
      ConnectionState::Closing(ClosingState) => {},
    };
  }

}
