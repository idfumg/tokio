#![allow(unused_variables)]
#![allow(dead_code)]

use std::io;
use std::net::SocketAddr;
use cpython::*;
use futures::unsync::mpsc;
use futures::{unsync, Async, AsyncSink, Stream, Future, Poll, Sink};
use bytes::{Bytes, BytesMut};
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_io::codec::{Encoder, Decoder, Framed};
use tokio_core::net::TcpStream;

use utils::{Classes, PyLogger, ToPyErr, with_py};
use pybytes;
use pyfuture::PyFuture;
use pyunsafe::{GIL, Handle, Sender};

// Transport factory
pub type TransportFactory = fn(Handle, &PyObject, TcpStream, Option<SocketAddr>)
                               -> io::Result<(PyObject, PyObject)>;

pub enum TcpTransportMessage {
    Bytes(PyBytes),
    Close,
}


pub fn tcp_transport_factory<T>(
    handle: Handle, factory: &PyObject,
    socket: T, _peer: Option<SocketAddr>) -> Result<(PyObject, PyObject), io::Error>

    where T: AsyncRead + AsyncWrite + 'static
{
    let gil = Python::acquire_gil();
    let py = gil.python();

    // create protocol
    let proto = factory.call(py, NoArgs, None).log_error(py, "Protocol factory failure")?;

    let (tx, rx) = mpsc::unbounded();
    let tr = PyTcpTransport::new(py, handle.clone(), Sender::new(tx), &proto)?;
    let conn_lost = tr.clone_ref(py);
    let conn_err = tr.clone_ref(py);

    // create transport and then call connection_made on protocol
    let transport = TcpTransport::new(socket, rx, tr.clone_ref(py));

    handle.spawn(
        transport.map(move |_| {
            conn_lost.connection_lost()
        }).map_err(move |err| {
            conn_err.connection_error(err)
        })
    );
    Ok((tr.into_object(), proto))
}


py_class!(pub class PyTcpTransport |py| {
    data _handle: Handle;
    data _connection_lost: PyObject;
    data _data_received: PyObject;
    data _transport: Sender<TcpTransportMessage>;

    def get_extra_info(&self, _name: PyString,
                       default: Option<PyObject> = None ) -> PyResult<PyObject> {
        Ok(
            if let Some(ob) = default {
                ob
            } else {
                py.None()
            }
        )
    }

    //
    // write bytes to transport
    //
    def write(&self, data: PyBytes) -> PyResult<PyObject> {
        //let bytes = Bytes::from(data.data(py));
        let _ = self._transport(py).send(TcpTransportMessage::Bytes(data));
        Ok(py.None())
    }

    //
    // write all data to socket
    //
    def drain(&self) -> PyResult<PyFuture> {
        let fut = PyFuture::new(py, self._handle(py).clone())?;
        fut.set_result(py, py.None())?;
        Ok(fut)
    }

    //
    // close transport
    //
    def close(&self) -> PyResult<PyObject> {
        let _ = self._transport(py).send(TcpTransportMessage::Close);
        Ok(py.None())
    }

});

impl PyTcpTransport {

    pub fn new(py: Python, h: Handle,
               sender: Sender<TcpTransportMessage>,
               protocol: &PyObject) -> PyResult<PyTcpTransport> {

        // get protocol callbacks
        let connection_made = protocol.getattr(py, "connection_made")?;
        let connection_lost = protocol.getattr(py, "connection_lost")?;
        let data_received = protocol.getattr(py, "data_received")?;

        let transport = PyTcpTransport::create_instance(
            py, h, connection_lost, data_received, sender)?;

        // connection made
        connection_made.call(
            py, PyTuple::new(
                py, &[transport.clone_ref(py).into_object()]), None)
            .log_error(py, "Protocol.connection_made error")?;

        Ok(transport)
    }

    pub fn connection_lost(&self) {
        trace!("Protocol.connection_lost(None)");
        with_py(|py| {
            self._connection_lost(py).call(py, PyTuple::new(py, &[py.None()]), None)
                .into_log(py, "connection_lost error");
        });
    }

    pub fn connection_error(&self, err: io::Error) {
        trace!("Protocol.connection_lost({:?})", err);
        with_py(|py| {
            match err.kind() {
                io::ErrorKind::TimedOut => {
                    trace!("socket.timeout");
                    with_py(|py| {
                        let e = Classes.SocketTimeout.call(
                            py, NoArgs, None).unwrap();

                        self._connection_lost(py).call(py, PyTuple::new(py, &[e]), None)
                            .into_log(py, "connection_lost error");
                    });
                },
                _ => {
                    trace!("Protocol.connection_lost(err): {:?}", err);
                    with_py(|py| {
                        let mut e = err.to_pyerr(py);
                        self._connection_lost(py).call(py,
                                                       PyTuple::new(py, &[e.instance(py)]), None)
                            .into_log(py, "connection_lost error");
                    });
                }
            }
        });
    }

    pub fn data_received(&self, bytes: Bytes) {
        with_py(|py| {
            let _ = pybytes::PyBytes::new(py, bytes)
                .map_err(|e| e.into_log(py, "can not create PyBytes"))
                .map(|bytes|
                     self._data_received(py).call(py, (bytes,).to_py_object(py), None)
                     .into_log(py, "data_received error"));
        });
    }

}


struct TcpTransport<T> {
    framed: Framed<T, TcpTransportCodec>,
    intake: unsync::mpsc::UnboundedReceiver<TcpTransportMessage>,
    transport: PyTcpTransport,

    buf: Option<PyBytes>,
    incoming_eof: bool,
    flushed: bool,
    closing: bool,
}

impl<T> TcpTransport<T>
    where T: AsyncRead + AsyncWrite
{

    fn new(socket: T,
           intake: mpsc::UnboundedReceiver<TcpTransportMessage>,
           transport: PyTcpTransport) -> TcpTransport<T> {

        TcpTransport {
            framed: socket.framed(TcpTransportCodec),
            intake: intake,
            transport: transport,

            buf: None,
            incoming_eof: false,
            flushed: false,
            closing: false,
        }
    }
}


impl<T> Future for TcpTransport<T>
    where T: AsyncRead + AsyncWrite
{
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // poll for incoming data
        if !self.incoming_eof {
            loop {
                match self.framed.poll() {
                    Ok(Async::Ready(Some(bytes))) => {
                        self.transport.data_received(bytes);
                        continue
                    },
                    Ok(Async::Ready(None)) => {
                        debug!("connectino_lost");
                        self.incoming_eof = true;
                    },
                    Ok(Async::NotReady) => (),
                    Err(err) => return Err(err.into())
                }
                break
            }
        }

        loop {
            let bytes = if let Some(bytes) = self.buf.take() {
                Some(bytes)
            } else {
                match self.intake.poll() {
                    Ok(Async::Ready(Some(msg))) => {
                        match msg {
                            TcpTransportMessage::Bytes(bytes) =>
                                Some(bytes),
                            TcpTransportMessage::Close =>
                                return Ok(Async::Ready(())),
                        }
                    }
                    Ok(_) => None,
                    Err(_) => return Err(io::Error::new(io::ErrorKind::Other, "Closed")),
                }
            };

            if let Some(bytes) = bytes {
                self.flushed = false;

                match self.framed.start_send(bytes) {
                    Ok(AsyncSink::NotReady(bytes)) => {
                        self.buf = Some(bytes);
                        break
                    }
                    Ok(AsyncSink::Ready) => continue,
                    Err(_) => return Err(io::Error::new(io::ErrorKind::Other, "Closed")),
                }
            } else {
                break
            }
        }

        // flush sink
        if !self.flushed {
            self.flushed = self.framed.poll_complete()?.is_ready();
        }

        Ok(Async::NotReady)
    }
}


struct TcpTransportCodec;

impl Decoder for TcpTransportCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if !src.is_empty() {
            Ok(Some(src.take().freeze()))
        } else {
            Ok(None)
        }
    }
}

impl Encoder for TcpTransportCodec {
    type Item = PyBytes;
    type Error = io::Error;

    fn encode(&mut self, msg: PyBytes, dst: &mut BytesMut) -> Result<(), Self::Error> {
        dst.extend(msg.data(GIL::python()));
        Ok(())
    }

}
