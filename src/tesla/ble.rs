use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread::JoinHandle;
use std::time::Duration;

use prost::Message;

use super::TeslaResult;
use super::proto::universal_message::RoutableMessage;

/// A persistent write connection to the Photon BLE bridge.
///
/// A background reader thread blocks indefinitely on the TCP socket. For each
/// message it calls `on_message(Some(msg))`; on close it calls `on_message(None)`
/// and exits. The caller owns the processing logic — this struct is just the
/// transport.
pub struct BleBridge {
    write_stream: TcpStream,
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

        let reader = std::thread::spawn(move || loop {
            match recv_raw(&mut read_stream) {
                Ok(msg) => on_message(Some(msg)),
                Err(_) => { on_message(None); break; }
            }
        });

        Ok(Self { write_stream, _reader: reader })
    }

    pub fn send(&mut self, msg: &RoutableMessage) -> TeslaResult<()> {
        let bytes = msg.encode_to_vec();
        self.write_stream.write_all(&(bytes.len() as u16).to_be_bytes())?;
        self.write_stream.write_all(&bytes)?;
        Ok(())
    }
}

fn recv_raw(stream: &mut TcpStream) -> TeslaResult<RoutableMessage> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf)?;
    let mut buf = vec![0u8; u16::from_be_bytes(len_buf) as usize];
    stream.read_exact(&mut buf)?;
    Ok(RoutableMessage::decode(buf.as_slice())?)
}
