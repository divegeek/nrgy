use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread::JoinHandle;
use std::time::Duration;

use log::{debug, warn};
use prost::Message;

use super::TeslaResult;
use super::proto::universal_message::RoutableMessage;

pub struct BleBridge {
    write_stream: TcpStream,
    failed: bool,
    _reader: JoinHandle<()>,
}

impl BleBridge {
    pub fn connect<F>(
        host: &str,
        port: u16,
        connect_timeout: Duration,
        on_message: F,
    ) -> TeslaResult<Self>
    where
        F: Fn(Option<RoutableMessage>) + Send + 'static,
    {
        let addr: SocketAddr = format!("{host}:{port}")
            .parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let stream = TcpStream::connect_timeout(&addr, connect_timeout)?;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(connect_timeout))?;

        let mut read_stream = stream.try_clone()?;
        let write_stream = stream;

        let reader = std::thread::spawn(move || {
            loop {
                match recv_raw(&mut read_stream) {
                    Ok(msg) => on_message(Some(msg)),
                    Err(_) => {
                        on_message(None);
                        break;
                    }
                }
            }
        });

        Ok(Self {
            write_stream,
            failed: false,
            _reader: reader,
        })
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    pub fn send(&mut self, msg: &RoutableMessage) -> TeslaResult<()> {
        let bytes = msg.encode_to_vec();
        let result = self
            .write_stream
            .write_all(&(bytes.len() as u16).to_be_bytes())
            .and_then(|_| self.write_stream.write_all(&bytes));
        if let Err(ref e) = result {
            debug!("BLE send failed ({e}), marking bridge as failed");
            self.failed = true;
        }
        Ok(result?)
    }

    pub fn set_failed(&mut self) {
        warn!("TCP connection failed");
        self.failed = true;
    }
}

fn recv_raw(stream: &mut TcpStream) -> TeslaResult<RoutableMessage> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf)?;
    let mut buf = vec![0u8; u16::from_be_bytes(len_buf) as usize];
    stream.read_exact(&mut buf)?;
    Ok(RoutableMessage::decode(buf.as_slice())?)
}
