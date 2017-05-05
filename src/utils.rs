#![allow(non_upper_case_globals)]

use cpython;
use cpython::*;
use std::io;
use std::os::raw::c_long;
use std::time::Duration;
use std::error::Error;
use std::fmt::Write;

use pyfuture::PyFuture;
use addrinfo::LookupError;


#[allow(non_snake_case)]
pub struct WorkingClasses {
    pub Future: PyType,

    pub CancelledError: PyType,
    pub InvalidStateError: PyType,
    pub TimeoutError: PyType,

    pub Exception: PyType,
    pub BaseException: PyType,
    pub StopIteration: PyType,

    pub Socket: PyModule,
    pub GaiError: PyType,
    pub SocketTimeout: PyType,
    pub GetNameInfo: PyObject,

    pub BrokenPipeError: PyType,
    pub ConnectionAbortedError: PyType,
    pub ConnectionRefusedError: PyType,
    pub ConnectionResetError: PyType,
    pub InterruptedError: PyType,

    pub Sys: PyModule,
    pub Asyncio: PyModule,
    pub Traceback: PyModule,
    pub ExtractStack: PyObject,
}

lazy_static! {
    pub static ref Classes: WorkingClasses = {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let builtins = py.import("builtins").unwrap();
        let socket = py.import("socket").unwrap();
        let tb = py.import("traceback").unwrap();
        let asyncio = py.import("asyncio").unwrap();

        WorkingClasses {
            // asyncio types
            Future: py.get_type::<PyFuture>(),

            Asyncio: asyncio.clone_ref(py),
            CancelledError: PyType::extract(
                py, &asyncio.get(py, "CancelledError").unwrap()).unwrap(),
            InvalidStateError: PyType::extract(
                py, &asyncio.get(py, "InvalidStateError").unwrap()).unwrap(),
            TimeoutError: PyType::extract(
                py, &asyncio.get(py, "TimeoutError").unwrap()).unwrap(),

            // general purpose types
            StopIteration: PyType::extract(
                py, &builtins.get(py, "StopIteration").unwrap()).unwrap(),
            Exception: PyType::extract(
                py, &builtins.get(py, "Exception").unwrap()).unwrap(),
            BaseException: PyType::extract(
                py, &builtins.get(py, "BaseException").unwrap()).unwrap(),

            BrokenPipeError: PyType::extract(
                py, &builtins.get(py, "BrokenPipeError").unwrap()).unwrap(),
            ConnectionAbortedError: PyType::extract(
                py, &builtins.get(py, "ConnectionAbortedError").unwrap()).unwrap(),
            ConnectionRefusedError: PyType::extract(
                py, &builtins.get(py, "ConnectionRefusedError").unwrap()).unwrap(),
            ConnectionResetError: PyType::extract(
                py, &builtins.get(py, "ConnectionResetError").unwrap()).unwrap(),
            InterruptedError: PyType::extract(
                py, &builtins.get(py, "InterruptedError").unwrap()).unwrap(),

            SocketTimeout: PyType::extract(
                py, &socket.get(py, "timeout").unwrap()).unwrap(),
            GaiError: PyType::extract(
                py, &socket.get(py, "gaierror").unwrap()).unwrap(),
            GetNameInfo: socket.get(py, "getnameinfo").unwrap(),
            Socket: socket,

            Sys: py.import("sys").unwrap(),
            Traceback: tb.clone_ref(py),
            ExtractStack: tb.get(py, "extract_stack").unwrap(),
        }
    };
}


// Temporarily acquire GIL
pub fn with_py<T, F>(f: F) -> T where F: FnOnce(Python) -> T {
    let gil = Python::acquire_gil();
    let py = gil.python();
    f(py)
}

pub fn iscoroutine(ob: &PyObject) -> bool {
    unsafe {
        (cpython::_detail::ffi::PyCoro_Check(ob.as_ptr()) != 0 ||
         cpython::_detail::ffi::PyCoroWrapper_Check(ob.as_ptr()) != 0 ||
         cpython::_detail::ffi::PyAsyncGen_Check(ob.as_ptr()) != 0 ||
         cpython::_detail::ffi::PyIter_Check(ob.as_ptr()) != 0 ||
         cpython::_detail::ffi::PyGen_Check(ob.as_ptr()) != 0)
    }
}


pub trait PyLogger {

    fn into_log(&self, py: Python, msg: &str);

    fn log_error(self, py: Python, msg: &str) -> Self;

}

impl<T> PyLogger for PyResult<T> {

    fn into_log(&self, py: Python, msg: &str) {
        if let &Err(ref err) = self {
            error!("{} {:?}", msg, err);
            err.clone_ref(py).print(py);
        }
    }

    fn log_error(self, py: Python, msg: &str) -> Self {
        match &self {
            &Err(ref err) => {
                error!("{} {:?}", msg, err);
                err.clone_ref(py).print(py);
            }
            _ => (),
        }
        self
    }
}


impl PyLogger for PyErr {

    fn into_log(&self, py: Python, msg: &str) {
        error!("{} {:?}", msg, self);
        self.clone_ref(py).print(py);
    }

    fn log_error(self, py: Python, msg: &str) -> Self {
        error!("{} {:?}", msg, self);
        self.clone_ref(py).print(py);
        self
    }
}


/// Converts into PyErr
pub trait ToPyErr {

    fn to_pyerr(&self, Python) -> PyErr;

}

/// Create OSError from io::Error
impl ToPyErr for io::Error {

    fn to_pyerr(&self, py: Python) -> PyErr {
        let tp;
        let exc_type = match self.kind() {
            io::ErrorKind::BrokenPipe => &Classes.BrokenPipeError,
            io::ErrorKind::ConnectionRefused => &Classes.ConnectionRefusedError,
            io::ErrorKind::ConnectionAborted => &Classes.ConnectionAbortedError,
            io::ErrorKind::ConnectionReset => &Classes.ConnectionResetError,
            io::ErrorKind::Interrupted => &Classes.InterruptedError,
            _ => {
                tp = py.get_type::<exc::OSError>();
                &tp
            }
        };

        let errno = self.raw_os_error().unwrap_or(0);
        let errdesc = self.description();

        PyErr::new_err(py, exc_type, (errno, errdesc))
    }
}


impl ToPyErr for LookupError {

    fn to_pyerr(&self, py: Python) -> PyErr {
        match self {
            &LookupError::IOError(ref err) => err.to_pyerr(py),
            &LookupError::Other(ref err_str) =>
                PyErr::new_err(py, &Classes.GaiError, (err_str,)),
            &LookupError::NulError(_) =>
                PyErr::new_err(py, &Classes.GaiError, ("nil pointer",)),
            &LookupError::Generic =>
                PyErr::new_err(py, &Classes.GaiError, ("generic error",)),
        }
    }
}


//
// Format exception
//
pub fn print_exception(py: Python, w: &mut String, err: PyErr) {
    let res = Classes.Traceback.call(py, "format_exception",
                                     (err.ptype, err.pvalue, err.ptraceback), None);
    if let Ok(lines) = res {
        if let Ok(lines) = PyList::downcast_from(py, lines) {
            for idx in 0..lines.len(py) {
                let _ = write!(w, "{}", lines.get_item(py, idx));
            }
        }
    }
}

//
// convert PyFloat or PyInt into Duration
//
pub fn parse_seconds(py: Python, name: &str, value: PyObject) -> PyResult<Option<Duration>> {
    if let Ok(f) = PyFloat::downcast_from(py, value.clone_ref(py)) {
        let val = f.value(py);
        if val < 0.0 {
            Ok(None)
        } else {
            Ok(Some(Duration::new(val as u64, (val.fract() * 1_000_000_000.0) as u32)))
        }
    } else if let Ok(i) = PyInt::downcast_from(py, value) {
        if let Ok(val) = i.as_object().extract::<c_long>(py) {
            if val < 0 {
                Ok(None)
            } else {
                Ok(Some(Duration::new(val as u64, 0)))
            }
        } else {
            Ok(None)
        }
    } else {
        Err(PyErr::new::<exc::TypeError, _>(
            py, format!("'{}' must be int of float type", name)))
    }
}


//
// convert PyFloat or PyInt into u64 (milliseconds)
//
pub fn parse_millis(py: Python, name: &str, value: PyObject) -> PyResult<u64> {
    if let Ok(f) = PyFloat::downcast_from(py, value.clone_ref(py)) {
        let val = f.value(py);
        if val > 0.0 {
            Ok((val * 1000.0) as u64)
        } else {
            Ok(0)
        }
    } else if let Ok(i) = PyInt::downcast_from(py, value) {
        if let Ok(val) = i.as_object().extract::<c_long>(py) {
            if val < 0 {
                Ok(0)
            } else {
                Ok((val * 1000) as u64)
            }
        } else {
            Ok(0)
        }
    } else {
        Err(PyErr::new::<exc::TypeError, _>(
            py, format!("'{}' must be int of float type", name)))
    }
}
