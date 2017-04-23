#![allow(unused_variables)]

use std::thread;
use std::net;
use std::error::Error;
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use cpython::*;
use boxfnonce::SendBoxFnOnce;
use futures::{future, Future, Stream};
use futures::sync::{oneshot};
use tokio_core::reactor::{Core, CoreId, Remote};
use native_tls::TlsConnector;
use tokio_signal;

use addrinfo;
use client;
use handle;
use http;
use ::{PyFuture, PyTask};
use server;
use transport;
use utils::{self, with_py, Classes, ToPyErr};
use pyunsafe::Handle;


thread_local!(
    pub static CORE: RefCell<Option<Core>> = RefCell::new(None);
);


pub fn no_loop_exc(py: Python) -> PyErr {
    let cur = thread::current();
    PyErr::new::<exc::RuntimeError, _>(
        py,
        format!("There is no current event loop in thread {}.",
                cur.name().unwrap_or("unknown")).to_py_object(py))
}


pub fn new_event_loop(py: Python) -> PyResult<TokioEventLoop> {
    CORE.with(|cell| {
        let core = Core::new().unwrap();

        let evloop = TokioEventLoop::create_instance(
            py, core.id(),
            Handle::new(core.handle()),
            Instant::now(),
            addrinfo::start_workers(5),
            RefCell::new(None),
            RefCell::new(py.None()),
            Cell::new(false));

        *cell.borrow_mut() = Some(core);
        evloop
    })
}


pub fn thread_safe_check(py: Python, id: &CoreId) -> Option<PyErr> {
    let check = CORE.with(|cell| {
        match *cell.borrow() {
            None => false,
            Some(ref core) => return core.id() == *id,
        }
    });

    if !check {
        Some(PyErr::new::<exc::RuntimeError, _>(
            py, PyString::new(
                py, "Non-thread-safe operation invoked on an event loop \
                     other than the current one")))
    } else {
        None
    }
}

#[derive(Debug)]
enum RunStatus {
    Stopped,
    CtrlC,
    NoEventLoop,
    Error
}


py_class!(pub class TokioEventLoop |py| {
    data id: CoreId;
    data handle: Handle;
    data instant: Instant;
    data _lookup: addrinfo::LookupWorkerSender;
    data _runner: RefCell<Option<oneshot::Sender<bool>>>;
    data _exception_handler: RefCell<PyObject>;
    data _debug: Cell<bool>;

    //
    // Create a Future object attached to the loop.
    //
    def create_future(&self) -> PyResult<PyFuture> {
        if self._debug(py).get() {
            if let Some(err) = thread_safe_check(py, &self.id(py)) {
                return Err(err)
            }
        }

        PyFuture::new(py, self.handle(py).clone())
    }

    //
    // Schedule a coroutine object.
    //
    // Return a task object.
    //
    def create_task(&self, coro: PyObject) -> PyResult<PyTask> {
        if self._debug(py).get() {
            if let Some(err) = thread_safe_check(py, &self.id(py)) {
                return Err(err)
            }
        }

        PyTask::new(py, coro, self.clone_ref(py).into_object(), self.handle(py).clone())
    }

    //
    // Return the time according to the event loop's clock.
    //
    // This is a float expressed in seconds since event loop creation.
    //
    def time(&self) -> PyResult<f64> {
        let time = self.instant(py).elapsed();
        Ok(time.as_secs() as f64 + (time.subsec_nanos() as f64 / 1_000_000.0))
    }

    //
    // Return the time according to the event loop's clock (milliseconds)
    //
    def millis(&self) -> PyResult<u64> {
        let time = self.instant(py).elapsed();
        Ok(time.as_secs() * 1000 + (time.subsec_nanos() as u64 / 1_000_000))
    }

    //
    // def call_soon(self, callback, *args):
    //
    // Arrange for a callback to be called as soon as possible.
    //
    // This operates as a FIFO queue: callbacks are called in the
    // order in which they are registered.  Each callback will be
    // called exactly once.
    //
    // Any positional arguments after the callback will be passed to
    // the callback when it is called.
    //
    def call_soon(&self, *args, **kwargs) -> PyResult<handle::TokioHandle> {
        if self._debug(py).get() {
            if let Some(err) = thread_safe_check(py, &self.id(py)) {
                return Err(err)
            }
        }

        let _ = utils::check_min_length(py, args, 1)?;

        // get params
        let callback = args.get_item(py, 0);

        handle::call_soon(
            py, &self.handle(py),
            callback, PyTuple::new(py, &args.as_slice(py)[1..]))
    }

    //
    // def call_later(self, delay, callback, *args)
    //
    // Arrange for a callback to be called at a given time.
    //
    // Return a Handle: an opaque object with a cancel() method that
    // can be used to cancel the call.
    //
    // The delay can be an int or float, expressed in seconds.  It is
    // always relative to the current time.
    //
    // Each callback will be called exactly once.  If two callbacks
    // are scheduled for exactly the same time, it undefined which
    // will be called first.

    // Any positional arguments after the callback will be passed to
    // the callback when it is called.
    //
    def call_later(&self, *args, **kwargs) -> PyResult<handle::TokioTimerHandle> {
        if self._debug(py).get() {
            if let Some(err) = thread_safe_check(py, &self.id(py)) {
                return Err(err)
            }
        }

        let _ = utils::check_min_length(py, args, 2)?;

        // get params
        let callback = args.get_item(py, 1);
        let delay = utils::parse_millis(py, "delay", args.get_item(py, 0))?;
        let when = Duration::from_millis(delay);

        handle::call_later(
            py, &self.handle(py),
            when, callback, PyTuple::new(py, &args.as_slice(py)[2..]))
    }

    //
    // def call_at(self, when, callback, *args):
    //
    // Like call_later(), but uses an absolute time.
    //
    // Absolute time corresponds to the event loop's time() method.
    //
    def call_at(&self, *args, **kwargs) -> PyResult<handle::TokioTimerHandle> {
        if self._debug(py).get() {
            if let Some(err) = thread_safe_check(py, &self.id(py)) {
                return Err(err)
            }
        }

        let _ = utils::check_min_length(py, args, 2)?;

        // get params
        let callback = args.get_item(py, 1);

        // calculate delay
        let when = utils::parse_seconds(py, "when", args.get_item(py, 0))?;
        let time = when - self.instant(py).elapsed();

        handle::call_later(
            py, &self.handle(py), time, callback, PyTuple::new(py, &args.as_slice(py)[2..]))
    }

    //
    // Stop running the event loop.
    //
    def stop(&self) -> PyResult<PyBool> {
        let runner = self._runner(py).borrow_mut().take();

        match runner  {
            Some(tx) => {
                let _ = tx.send(true);
                Ok(py.True())
            },
            None => Ok(py.False()),
        }
    }

    def is_running(&self) -> PyResult<bool> {
        Ok(match *self._runner(py).borrow() {
            Some(_) => true,
            None => false,
        })
    }

    def is_closed(&self) -> PyResult<bool> {
        CORE.with(|cell| {
            match cell.try_borrow() {
                Ok(ref cell) => Ok(cell.is_some()),
                Err(_) => Ok(true)
            }
        })
    }

    //
    // Close the event loop. The event loop must not be running.
    //
    def close(&self) -> PyResult<PyObject> {
        if let Ok(running) = self.is_running(py) {
            if running {
                return Err(
                    PyErr::new::<exc::RuntimeError, _>(
                        py, "Cannot close a running event loop"));
            }
        }

        CORE.with(|cell| {
            cell.borrow_mut().take()
        });

        Ok(py.None())
    }

    // return list of tuples
    // item = (family, type, proto, canonname, sockaddr)
    // sockaddr(IPV4) = (address, port)
    // sockaddr(IPV6) = (address, port, flow info, scope id)
    def getaddrinfo(&self, *args, **kwargs) -> PyResult<PyFuture> {
        let _ = utils::check_min_length(py, args, 2)?;

        // get params
        let host = PyString::downcast_from(py, args.get_item(py, 0))?;
        let port: u16 = args.get_item(py, 1).extract(py)?;

        let family: i32;
        let socktype: i32;
        // let proto: i32;
        let flags: i32;

        if let Some(kwargs) = kwargs {
            family = kwargs.get_item(py, "family")
                .map(|inst| inst.extract(py)).unwrap_or(Ok(0))?;
            socktype = kwargs.get_item(py, "family")
                .map(|inst| inst.extract(py)).unwrap_or(Ok(0))?;
            // proto = kwargs.get_item(py, "proto")
            //    .map(|inst| inst.extract(py)).unwrap_or(Ok(0))?;
            flags = kwargs.get_item(py, "flags")
                .map(|inst| inst.extract(py)).unwrap_or(Ok(0))?;
        } else {
            family = 0;
            socktype = 0;
            // proto = 0;
            flags = 0;
        }

        // result future
        let res = PyFuture::new(py, self.handle(py).clone())?;

        // create processing future
        let fut = res.clone_ref(py);
        let fut_err = res.clone_ref(py);

        // lookup process
        let lookup = addrinfo::lookup(
            &self._lookup(py),
            String::from(host.to_string(py)?.as_ref()),
            port, family, flags, addrinfo::SocketType::from_int(socktype));

        let process = lookup.and_then(move |result| {
            with_py(|py| match result {
                Err(ref err) => {
                    let _ = fut.set(py, Err(err.to_pyerr(py)));
                },
                Ok(ref addrs) => {
                    // create socket.gethostname compatible result
                    let list = PyList::new(py, &[]);
                    for info in addrs {
                        let addr = match info.sockaddr {
                            net::SocketAddr::V4(addr) => {
                                (format!("{}", addr.ip()).to_py_object(py),
                                 addr.port().to_py_object(py))
                                    .to_py_object(py)
                            }
                            net::SocketAddr::V6(addr) => {
                                (format!("{}", addr.ip()).to_py_object(py),
                                 addr.port().to_py_object(py),
                                 addr.flowinfo().to_py_object(py),
                                 addr.scope_id().to_py_object(py),)
                                    .to_py_object(py)
                            },
                        };

                        let cname = match info.canonname {
                            Some(ref cname) => PyString::new(py, cname.as_str()),
                            None => PyString::new(py, ""),
                        };

                        let item = (info.family.to_int().to_py_object(py),
                                    info.socktype.to_int().to_py_object(py),
                                    info.protocol.to_int().to_py_object(py),
                                    cname,
                                    addr).to_py_object(py).into_object();
                        list.insert_item(py, list.len(py), item);
                    }
                    println!("addrinfo: {}", list.clone_ref(py).into_object());
                    let _ = fut.set(py, Ok(list.into_object()));
                },
            });
            future::ok(())
        }).map_err(move |err| {
            with_py(|py| {
                let err = PyErr::new::<exc::RuntimeError, _>(py, "Unknown runtime error");
                fut_err.set(py, Err(err))
            });
        });

        // start task
        self.handle(py).spawn(process);

        Ok(res)
    }

    //
    // Create a TCP server.
    //
    // The host parameter can be a string, in that case the TCP server is bound
    // to host and port.
    //
    // The host parameter can also be a sequence of strings and in that case
    // the TCP server is bound to all hosts of the sequence. If a host
    // appears multiple times (possibly indirectly e.g. when hostnames
    // resolve to the same IP address), the server is only bound once to that
    // host.
    //
    // Return a Server object which can be used to stop the service.
    //
    def create_server(&self, protocol_factory: PyObject,
                      host: Option<PyString>, port: Option<u16> = None,
                      family: i32 = 0,
                      flags: i32 = addrinfo::AI_PASSIVE,
                      sock: Option<PyObject> = None,
                      backlog: i32 = 100,
                      ssl: Option<PyObject> = None,
                      reuse_address: bool = true,
                      reuse_port: bool = true) -> PyResult<PyFuture> {

        if let Some(ssl) = ssl {
            return Err(PyErr::new::<exc::TypeError, _>(
                py, PyString::new(py, "ssl argument is not supported yet")));
        }

        server::create_server(
            py, protocol_factory, self.handle(py).clone(),
            Some(String::from(host.unwrap().to_string_lossy(py))), Some(port.unwrap_or(0)),
            family, flags, sock, backlog, ssl, reuse_address, reuse_port,
            transport::tcp_transport_factory)
    }

    def create_http_server(&self, protocol_factory: PyObject,
                           host: Option<PyString>, port: Option<u16> = None,
                           family: i32 = 0,
                           flags: i32 = addrinfo::AI_PASSIVE,
                           sock: Option<PyObject> = None,
                           backlog: i32 = 100,
                           ssl: Option<PyObject> = None,
                           reuse_address: bool = true,
                           reuse_port: bool = true) -> PyResult<PyFuture> {
        if let Some(ssl) = ssl {
            return Err(PyErr::new::<exc::ValueError, _>(
                py, PyString::new(py, "ssl argument is not supported yet")));
        }

        server::create_server(
            py, protocol_factory, self.handle(py).clone(),
            Some(String::from(host.unwrap().to_string_lossy(py))), Some(port.unwrap_or(0)),
            family, flags, sock, backlog, ssl, reuse_address, reuse_port,
            http::http_transport_factory)
    }

    // Connect to a TCP server.
    //
    // Create a streaming transport connection to a given Internet host and
    // port: socket family AF_INET or socket.AF_INET6 depending on host (or
    // family if specified), socket type SOCK_STREAM. protocol_factory must be
    // a callable returning a protocol instance.
    //
    // This method is a coroutine which will try to establish the connection
    // in the background.  When successful, the coroutine returns a
    // (transport, protocol) pair.
    //
    def create_connection(&self, protocol_factory: PyObject,
                          host: Option<PyString>, port: Option<u16> = None,
                          ssl: Option<PyObject> = None,
                          family: i32 = 0, proto: i32 = 0,
                          flags: i32 = addrinfo::AI_PASSIVE,
                          sock: Option<PyObject> = None,
                          local_addr: Option<PyObject> = None,
                          server_hostname: Option<PyString> = None) -> PyResult<PyFuture> {
        match (&server_hostname, &ssl) {
            (&Some(_), &None) =>
                return Err(PyErr::new::<exc::ValueError, _>(
                    py, "server_hostname is only meaningful with ssl".to_py_object(py))),
            (&None, &Some(_)) => {
                // Use host as default for server_hostname.  It is an error
                // if host is empty or not set, e.g. when an
                // already-connected socket was passed or when only a port
                // is given.  To avoid this error, you can pass
                // server_hostname='' -- this will bypass the hostname
                // check.  (This also means that if host is a numeric
                // IP/IPv6 address, we will attempt to verify that exact
                // address; this will probably fail, but it is possible to
                // create a certificate for a specific IP address, so we
                // don't judge it here.)
                if let None = host {
                    return Err(PyErr::new::<exc::ValueError, _>(
                        py, "You must set server_hostname when using ssl without a host".to_py_object(py)));
                }
            }
            // server_hostname = host
            _ => (),
        }

        // create_ssl context
        let ctx =
            if let Some(ssl) = ssl {
                match TlsConnector::builder() {
                    Err(err) =>
                        return Err(PyErr::new_lazy_init(
                            Classes.OSError.clone_ref(py),
                            Some(err.description().to_py_object(py).into_object()))),
                    Ok(builder) => match builder.build() {
                        Err(err) =>
                            return Err(PyErr::new_lazy_init(
                                Classes.OSError.clone_ref(py),
                                Some(err.description().to_py_object(py).into_object()))),
                        Ok(ctx) => Some(ctx)
                    },
                }
            } else {
                None
            };

        match (&host, &port) {
            (&None, &None) => {
                if let Some(_) = sock {
                    Err(PyErr::new::<exc::ValueError, _>(
                        py, PyString::new(py, "sock is not supported yet")))
                } else {
                    Err(PyErr::new::<exc::ValueError, _>(
                        py, "host and port was not specified and no sock specified"
                            .to_py_object(py)))
                }
            },
            _ => {
                if let Some(_) = sock {
                    return Err(PyErr::new::<exc::ValueError, _>(
                        py, "host/port and sock can not be specified at the same time"
                            .to_py_object(py)))
                }

                // exctract hostname
                let host = host.map(|s| String::from(s.to_string_lossy(py)))
                    .unwrap_or(String::new());

                // server hostname for ssl validation
                let server_hostname = match server_hostname {
                    Some(s) => String::from(s.to_string(py)?),
                    None => host.clone(),
                };

                let fut = PyFuture::new(py, self.handle(py).clone())?;

                // resolve addresses
                let lookup = addrinfo::lookup(
                    &self._lookup(py),
                    host, port.unwrap_or(0), family, flags, addrinfo::SocketType::Stream);

                let handle = self.handle(py).clone();
                let fut_err = fut.clone_ref(py);
                let fut_conn = fut.clone_ref(py);

                // connect
                let conn = lookup
                    .map_err(move |_| {
                        let _ = with_py(|py| fut_err.cancel(py));
                    })
                    .and_then(move |result| with_py(|py| {
                        let res = result;
                        match res {
                            Err(err) => {
                                let _ = fut_conn.set(py, Err(err.to_pyerr(py)));
                                future::ok(())
                            },
                            Ok(addrs) => {
                                if addrs.is_empty() {
                                    let _ = fut_conn.set(
                                        py,
                                        Err(PyErr::new_lazy_init(
                                            Classes.OSError.clone_ref(py),
                                            Some("getaddrinfo() returned empty list"
                                                 .to_py_object(py).into_object())))
                                    );
                                    future::ok(())
                                } else {
                                    client::create_connection(
                                        py, protocol_factory,
                                        handle, fut_conn, addrs, ctx, server_hostname);
                                    future::ok(())
                                }
                            }
                        }}));
                self.handle(py).spawn(conn);

                Ok(fut)
            },
        }
    }

    // Return an exception handler, or None if the default one is in use.
    def get_exception_handler(&self) -> PyResult<PyObject> {
        Ok(self._exception_handler(py).borrow().clone_ref(py))
    }

    // Set handler as the new event loop exception handler.
    //
    // If handler is None, the default exception handler will
    // be set.
    //
    // If handler is a callable object, it should have a
    // signature matching '(loop, context)', where 'loop'
    // will be a reference to the active event loop, 'context'
    // will be a dict object (see `call_exception_handler()`
    // documentation for details about context).
    def set_exception_handler(&self, handler: PyObject) -> PyResult<PyObject> {
        if !handler.is_callable(py) {
            Err(PyErr::new::<exc::TypeError, _>(
                py, format!("A callable object or None is expected, got {:?}",
                            handler).to_py_object(py)))
        } else {
            *self._exception_handler(py).borrow_mut() = handler;
            Ok(py.None())
        }
    }

    // Call the current event loop's exception handler.
    //
    // The context argument is a dict containing the following keys:
    //
    // - 'message': Error message;
    // - 'exception' (optional): Exception object;
    // - 'future' (optional): Future instance;
    // - 'handle' (optional): Handle instance;
    // - 'protocol' (optional): Protocol instance;
    // - 'transport' (optional): Transport instance;
    // - 'socket' (optional): Socket instance;
    // - 'asyncgen' (optional): Asynchronous generator that caused
    //                          the exception.
    //
    // New keys maybe introduced in the future.
    //
    // Note: do not overload this method in an event loop subclass.
    // For custom exception handling, use the `set_exception_handler()` method.
    def call_exception_handler(&self, context: PyDict) -> PyResult<PyObject> {
        let handler = self._exception_handler(py).borrow();
        if *handler == py.None() {
            error!("Unhandled error in ecent loop, context: {}", context.into_object());
        } else {
            let res = handler.call(py, (context.clone_ref(py),).to_py_object(py), None);
            if let Err(err) = res {
                // Exception in the user set custom exception handler.
                error!(
                    "Unhandled error in exception handler, exception: {:?}, context: {}",
                    err, context.into_object());
            }
        }
        Ok(py.None())
    }

    //
    // Run until stop() is called
    //
    def run_forever(&self, stop_on_sigint: bool = true) -> PyResult<PyObject> {
        let res = py.allow_threads(|| {
            CORE.with(|cell| {
                match *cell.borrow_mut() {
                    Some(ref mut core) => {
                        let rx = {
                            let gil = Python::acquire_gil();
                            let py = gil.python();

                            // set cancel sender
                            let (tx, rx) = oneshot::channel::<bool>();
                            *(self._runner(py)).borrow_mut() = Some(tx);
                            rx
                        };

                        // SIGINT
                        if stop_on_sigint {
                            let ctrlc_f = tokio_signal::ctrl_c(&core.handle());
                            let ctrlc = core.run(ctrlc_f).unwrap().into_future();

                            let fut = rx.select2(ctrlc).then(|res| {
                                match res {
                                    Ok(future::Either::A(_)) => future::ok(RunStatus::Stopped),
                                    Ok(future::Either::B(_)) => future::ok(RunStatus::CtrlC),
                                    Err(e) => future::err(()),
                                }
                            });
                            match core.run(fut) {
                                Ok(status) => status,
                                Err(_) => RunStatus::Error,
                            }
                        } else {
                            match core.run(rx) {
                                Ok(_) => RunStatus::Stopped,
                                Err(_) => RunStatus::Error,
                            }
                        }
                    }
                    None => RunStatus::NoEventLoop,
                }
            })
        });

        let _ = self.stop(py);

        match res {
            RunStatus::Stopped => Ok(py.None()),
            RunStatus::CtrlC => Ok(py.None()),
            RunStatus::Error => Err(
                PyErr::new::<exc::RuntimeError, _>(py, "Unknown runtime error")),
            RunStatus::NoEventLoop => Err(no_loop_exc(py)),
        }
    }

    //
    // Run until the Future is done.
    //
    // If the argument is a coroutine, it is wrapped in a Task.
    //
    // WARNING: It would be disastrous to call run_until_complete()
    // with the same coroutine twice -- it would wrap it in two
    // different Tasks and that can't be good.
    //
    // Return the Future's result, or raise its exception.
    //
    def run_until_complete(&self, future: PyObject) -> PyResult<PyObject> {
        let fut = match PyTask::downcast_from(py, future.clone_ref(py)) {
            Ok(fut) => fut,
            Err(_) => PyTask::new(py, future,
                                  self.clone_ref(py).into_object(), self.handle(py).clone())?,
        };

        let res = py.allow_threads(|| {
            CORE.with(|cell| {
                match *cell.borrow_mut() {
                    Some(ref mut core) => {
                        let (rx, done_rx) = {
                            let gil = Python::acquire_gil();
                            let py = gil.python();

                            // wait for future completion
                            let (done, done_rx) = oneshot::channel::<bool>();
                            fut.add_callback(py, SendBoxFnOnce::from(move |fut| {
                                let _ = done.send(true);
                            }));

                            // stop fut
                            let (tx, rx) = oneshot::channel::<bool>();
                            *(self._runner(py)).borrow_mut() = Some(tx);

                            (rx, done_rx)
                        };

                        // SIGINT
                        let ctrlc_f = tokio_signal::ctrl_c(&core.handle());
                        let ctrlc = core.run(ctrlc_f).unwrap().into_future();

                        // wait for completion
                        let _ = core.run(rx.select2(done_rx).select2(ctrlc));

                        true
                    }
                    None => false,
                }
            })
        });

        if res {
            // cleanup running state
            let _ = self.stop(py);

            fut.result(py)
        } else {
            Err(no_loop_exc(py))
        }
    }


    //
    // Event loop debug flag
    //
    def get_debug(&self) -> PyResult<bool> {
        Ok(self._debug(py).get())
    }

    //
    // Set event loop debug flag
    //
    def set_debug(&self, enabled: bool) -> PyResult<PyObject> {
        self._debug(py).set(enabled);
        Ok(py.None())
    }

});


impl TokioEventLoop {

    pub fn remote(&self, py: Python) -> Remote {
        self.handle(py).remote().clone()
    }

}
