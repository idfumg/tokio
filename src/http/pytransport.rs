#![allow(unused_variables)]
#![allow(dead_code)]

use std::io;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use cpython::*;
use futures::unsync::mpsc;
use futures::{Async, Future, Poll};

use future::{create_task, done_future, TokioFuture};
use http::{self, pyreq, codec};
use http::pyreq::{PyRequest, StreamReader};
use utils::{Classes, PyLogger, ToPyErr, with_py};
use pyunsafe::{GIL, Handle, Sender};


pub enum PyHttpTransportMessage {
    Close(Option<PyErr>),
}

const CONCURENCY_LEVEL: usize = 1;


py_class!(pub class PyHttpTransport |py| {
    data _loop: Handle;
    data _connection_lost: PyObject;
    data _data_received: PyObject;
    data _request_handler: PyObject;
    data transport: Sender<PyHttpTransportMessage>;
    data req: RefCell<Option<pyreq::PyRequest>>;
    data req_count: Cell<usize>;

    data inflight: Cell<usize>;
    data reqs: RefCell<VecDeque<(http::Request, Sender<codec::EncoderMessage>)>>;
    data payloads: RefCell<VecDeque<StreamReader>>;

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
        Err(PyErr::new::<exc::RuntimeError, _>(
            py, "write() method is not available, use PayloadWriter"))
    }

    //
    // send buffered data to socket
    //
    def drain(&self) -> PyResult<TokioFuture> {
        Ok(done_future(py, self._loop(py).clone(), py.None())?)
    }

    //
    // close transport
    //
    def close(&self) -> PyResult<PyObject> {
        let _ = self.transport(py).send(PyHttpTransportMessage::Close(None));
        Ok(py.None())
    }

});


impl PyHttpTransport {

    pub fn get_data_received(&self, py: Python) -> PyObject {
        self._data_received(py).clone_ref(py)
    }

    pub fn new(py: Python, h: Handle,
               sender: Sender<PyHttpTransportMessage>,
               factory: &PyObject) -> PyResult<PyHttpTransport> {
        // create protocol
        let proto = factory.call(py, NoArgs, None)
            .log_error(py, "Protocol factory error")?;

        // get protocol callbacks
        let connection_made = proto.getattr(py, "connection_made")?;
        let connection_lost = proto.getattr(py, "connection_lost")?;
        let data_received = proto.getattr(py, "data_received")?;
        let request_handler = proto.getattr(py, "handle_request")?;
        //let request_handler = proto.getattr(py, "_request_handler")?;

        let transport = PyHttpTransport::create_instance(
            py, h, connection_lost, data_received, request_handler, sender,
            RefCell::new(None), Cell::new(0), Cell::new(0),
            RefCell::new(VecDeque::with_capacity(12)),
            RefCell::new(VecDeque::with_capacity(CONCURENCY_LEVEL)))?;

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
            self.reqs(py).borrow_mut().clear();

            self._connection_lost(py).call(py, PyTuple::new(py, &[py.None()]), None)
                .into_log(py, "connection_lost error");
        });
    }

    pub fn connection_error(&self, err: io::Error) {
        trace!("Protocol.connection_lost({:?})", err);
        with_py(|py| {
            self.reqs(py).borrow_mut().clear();

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

    pub fn data_received(&self, msg: http::RequestMessage)
                         -> PyResult<Option<mpsc::UnboundedReceiver<codec::EncoderMessage>>> {
        match msg {
            http::RequestMessage::Message(msg) => {
                let (sender, recv) = mpsc::unbounded();

                with_py(|py| match pyreq::PyRequest::new(
                    py, msg, self._loop(py).clone(), Sender::new(sender)) {
                    Err(err) => {
                        error!("{:?}", err);
                        err.clone_ref(py).print(py);
                    },
                    Ok(req) => {
                        req.content().feed_eof(py);
                        self._data_received(py).call(
                            py, PyTuple::new(py, &[req.into_object()]), None)
                            .into_log(py, "data_received error");
                    }
                });
                return Ok(Some(recv));

                let py = GIL::python();
                let count = self.req_count(py);
                count.set(count.get() + 1);

                let inflight = self.inflight(py);
                if inflight.get() < CONCURENCY_LEVEL {
                    inflight.set(inflight.get() + 1);

                    // start handler task
                    let tx = self.transport(py).clone();
                    let handler = RequestHandler::new(
                        self._loop(py).clone(), msg, Sender::new(sender),
                        self.clone_ref(py), self._request_handler(py).clone_ref(py))?;

                    self._loop(GIL::python()).spawn(handler.map_err(move |err| {
                        // close connection with error
                        let _ = tx.send(PyHttpTransportMessage::Close(Some(err)));
                    }));
                } else {
                    //println!("wait");
                    self.reqs(py).borrow_mut().push_back((msg, Sender::new(sender)));
                }
                return Ok(Some(recv));
            },
            http::RequestMessage::Body(chunk) => {

            },
            http::RequestMessage::Completed => {
                //with_py(|py| {
                //    if let Some(payload) = self.payloads(py).borrow_mut().pop_front() {
                //        payload.feed_eof(py);
                //    } else {
                        //println!("not found");
                //    }
                //});
            }
        };
        Ok(None)
    }
}


struct RequestHandler {
    h: Handle,
    tr: PyHttpTransport,
    handler: PyObject,
    task: TokioFuture,
    inflight: PyRequest,
}

impl RequestHandler {

    fn new(h: Handle, msg: http::Request, tx: Sender<codec::EncoderMessage>,
           tr: PyHttpTransport, handler: PyObject) -> PyResult<RequestHandler> {

        let (task, req) = RequestHandler::start_task(h.clone(), msg, tx, &handler)?;

        Ok(RequestHandler {
            h: h,
            tr: tr,
            handler: handler,
            task: task,
            inflight: req,
        })
    }

    pub fn start_task(h: Handle, msg: http::Request,
                      sender: Sender<codec::EncoderMessage>,
                      handler: &PyObject) -> PyResult<(TokioFuture, PyRequest)> {
        // start python task
        with_py(|py| {
            let req = pyreq::PyRequest::new(py, msg, h.clone(), sender)?;
            req.content().feed_eof(py);

            let coro = handler.call(
                py, PyTuple::new(py, &[req.clone_ref(py).into_object()]), None)?;

            let task = create_task(py, coro, h)?;
            Ok((task, req))
        })
    }
}


impl Future for RequestHandler {
    type Item = ();
    type Error = PyErr;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let result = self.task.poll();

        // process result from python task
        match result {
            Ok(Async::Ready(res)) => {
                // select next message
                //if self.tr.reqs(GIL::python()).borrow_mut().len() > 0 {
                //    println!("num: {}", self.tr.reqs(GIL::python()).borrow_mut().len());
                //}
                let (msg, sender) = match self.tr.reqs(GIL::python()).borrow_mut().pop_front() {
                    Some((msg, sender)) => (msg, sender),
                    None => {
                        // nothing to process, decrease number of inflight tasks and exit
                        let inflight = self.tr.inflight(GIL::python());
                        inflight.set(inflight.get() - 1);

                        //println!("no requests in queue");
                        return Ok(Async::Ready(()))
                    }
                };
                let (task, req) = RequestHandler::start_task(
                    self.h.clone(), msg, sender, &self.handler)?;
                self.inflight = req;
                self.task = task;
                self.poll()
            }
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(err) => {
                // close connection with error
                Ok(Async::Ready(()))
            }
        }
    }
}
