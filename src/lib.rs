//! librsync bindings for Rust.
//!
//! This library contains bindings to librsync[1], to support computation and application of
//! network deltas, used in rsync and duplicity backup applications. This library encapsulates the
//! algorithms of the rsync protocol, which computes differences between files efficiently.
//!
//! The rsync protocol, when computes differences, does not require the presence of both files.
//! It needs instead the new file and a set of checksums of the first file (namely the signature).
//! Computed differences can be stored in a delta file. The rsync protocol is then able to
//! reproduce the new file, by having the old one and the delta.
//!
//! [1]: http://librsync.sourcefrog.net/
//!
//!
//! # Overview of types and modules
//!
//! This crate provides the streaming operations to produce signatures, delta and patches in the
//! top-level module, with `Signature`, `Delta` and `Patch` structs. Those structs take some input
//! stream (`Read` or `Read + Seek` traits) and implement another stream (`Read` trait) from which
//! the output can be read.
//!
//! Higher level operations are provided within the `whole` submodule. If the application does not
//! need fine-grained control over IO operations, `sig`, `delta` and `patch` submodules can be
//! used. Those functions apply the algorithms to an output stream (implementing the `Write` trait)
//! in a single call.
//!
//!
//! # Example: streams
//!
//! This example shows how to go trough the streaming APIs, starting from an input string and a
//! modified string which act as old and new files.
//!
//! ```rust
//! use std::io::prelude::*;
//! use std::io::Cursor;
//! use librsync::{Delta, Patch, Signature, SignatureType};
//!
//! let base = "base file".as_bytes();
//! let new = "base file (modified)".as_bytes();
//!
//! // create signature starting from base file
//! let sig = Signature::new(base, 10, 5, SignatureType::Blake2).unwrap();
//! // create delta from new file and the base signature
//! let delta = Delta::new(new, sig).unwrap();
//! // create and store the new file from the base one and the delta
//! let mut patch = Patch::new(Cursor::new(base), delta).unwrap();
//! let mut computed_new = Vec::new();
//! patch.read_to_end(&mut computed_new).unwrap();
//!
//! // test whether the computed file is exactly the new file, as expected
//! assert_eq!(computed_new, new);
//! ```
//!
//! Note that intermediate results are not stored anywhere. For example the `Signature` type is
//! directly used to feed the delta operation. This is possible because of the streaming fashon of
//! the operations.

#![deny(missing_copy_implementations,
        trivial_casts, trivial_numeric_casts,
        unstable_features,
        unused_import_braces, unused_qualifications)]

#![cfg_attr(feature = "nightly", allow(unstable_features))]
#![cfg_attr(feature = "lints", feature(plugin))]
#![cfg_attr(feature = "lints", plugin(clippy))]

extern crate librsync_sys as raw;
extern crate libc;
#[cfg(feature = "log")]
#[macro_use]
extern crate log;

mod macros;
mod job;
mod logfwd;
pub mod whole;

use job::{Job, JobDriver};

use std::error;
use std::fmt::{self, Display, Formatter};
use std::io::{self, Read, Seek};
use std::ops::Deref;
use std::mem;
use std::ptr;
use std::slice;


#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignatureType {
    MD4,
    Blake2,
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Syntax,
    Mem,
    BadMagic,
    Unimplemented,
    Internal,
    Unknown(i32),
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Signature<R> {
    driver: JobDriver<R>,
}

pub struct Delta<R> {
    driver: JobDriver<R>,
    _sumset: Sumset,
}

pub struct Patch<'a, R> {
    driver: JobDriver<R>,
    _base: Box<StreamHolder<'a>>,
}


struct Sumset(*mut raw::rs_signature_t);

struct StreamHolder<'a>(Box<ReadAndSeek + 'a>);

// workaround for E0225
trait ReadAndSeek: Read + Seek {}
impl<T: Read + Seek> ReadAndSeek for T {}


impl<R: Read> Signature<R> {
    pub fn new(input: R,
               block_len: usize,
               strong_len: usize,
               sig_magic: SignatureType)
               -> Result<Self> {
        logfwd::init();
        let job = unsafe { raw::rs_sig_begin(block_len, strong_len, sig_magic.as_raw()) };
        if job.is_null() {
            return Err(Error::BadMagic);
        }
        Ok(Signature { driver: JobDriver::new(input, Job(job)) })
    }

    pub fn into_inner(self) -> R {
        self.driver.into_inner()
    }
}

impl<R: Read> Read for Signature<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.driver.read(buf)
    }
}


impl<R: Read> Delta<R> {
    pub fn new<S: Read>(new: R, base_sig: S) -> Result<Self> {
        logfwd::init();
        // load the signature
        let sumset = unsafe {
            let mut sumset = ptr::null_mut();
            let job = raw::rs_loadsig_begin(&mut sumset);
            assert!(!job.is_null());
            let mut job = JobDriver::new(base_sig, Job(job));
            try!(job.consume_input());
            let sumset = Sumset(sumset);
            let res = raw::rs_build_hash_table(*sumset);
            if res != raw::RS_DONE {
                return Err(Error::from(res));
            }
            sumset
        };
        let job = unsafe { raw::rs_delta_begin(*sumset) };
        if job.is_null() {
            return Err(io_err(io::ErrorKind::InvalidData, "invalid signature given"));
        }
        Ok(Delta {
            driver: JobDriver::new(new, Job(job)),
            _sumset: sumset,
        })
    }

    pub fn into_inner(self) -> R {
        self.driver.into_inner()
    }
}

impl<R: Read> Read for Delta<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.driver.read(buf)
    }
}


impl<'a, R: Read> Patch<'a, R> {
    pub fn new<B: Read + Seek + 'a>(base: B, delta: R) -> Result<Self> {
        logfwd::init();

        let mut baseh = Box::new(StreamHolder(Box::new(base)));
        let job = unsafe { raw::rs_patch_begin(patch_copy_cb, baseh.as_raw()) };
        assert!(!job.is_null());
        Ok(Patch {
            driver: JobDriver::new(delta, Job(job)),
            _base: baseh,
        })
    }

    pub fn into_delta(self) -> R {
        self.driver.into_inner()
    }
}

impl<'a, R: Read> Read for Patch<'a, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.driver.read(buf)
    }
}


impl error::Error for Error {
    fn description(&self) -> &str {
        match *self {
            Error::Io(ref err) => err.description(),
            Error::Syntax => "syntax error",
            Error::Mem => "out of memory",
            Error::BadMagic => "bad magic number given",
            Error::Unimplemented => "unimplemented feature",
            Error::Internal => "internal error",
            Error::Unknown(_) => "unknown error from librsync",
        }
    }
}

impl Display for Error {
    fn fmt(&self, fmt: &mut Formatter) -> fmt::Result {
        match *self {
            Error::Io(ref e) => write!(fmt, "{}", e),
            Error::Unknown(n) => write!(fmt, "unknown error {} from native library", n),
            _ => write!(fmt, "{}", std::error::Error::description(self)),
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::Io(err)
    }
}

impl From<raw::rs_result> for Error {
    fn from(err: raw::rs_result) -> Error {
        match err {
            raw::RS_BLOCKED => io_err(io::ErrorKind::WouldBlock, "blocked waiting for more data"),
            raw::RS_IO_ERROR => io_err(io::ErrorKind::Other, "unknown IO error from librsync"),
            raw::RS_SYNTAX_ERROR => Error::Syntax,
            raw::RS_MEM_ERROR => Error::Mem,
            raw::RS_INPUT_ENDED => {
                io_err(io::ErrorKind::UnexpectedEof, "unexpected end of input file")
            }
            raw::RS_BAD_MAGIC => Error::BadMagic,
            raw::RS_UNIMPLEMENTED => Error::Unimplemented,
            raw::RS_CORRUPT => io_err(io::ErrorKind::InvalidData, "unbelievable value in stream"),
            raw::RS_INTERNAL_ERROR => Error::Internal,
            raw::RS_PARAM_ERROR => io_err(io::ErrorKind::InvalidInput, "bad parameter"),
            n => Error::Unknown(n),
        }
    }
}


impl SignatureType {
    fn as_raw(&self) -> raw::rs_magic_number {
        match *self {
            SignatureType::MD4 => raw::RS_MD4_SIG_MAGIC,
            SignatureType::Blake2 => raw::RS_BLAKE2_SIG_MAGIC,
        }
    }
}


impl Drop for Sumset {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_null() {
                raw::rs_free_sumset(self.0);
            }
        }
    }
}

impl Deref for Sumset {
    type Target = *mut raw::rs_signature_t;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}


impl<'a> StreamHolder<'a> {
    fn as_stream(&mut self) -> &mut ReadAndSeek {
        &mut *self.0
    }

    fn as_raw(&mut self) -> *mut libc::c_void {
        unsafe { mem::transmute(self) }
    }
}


extern "C" fn patch_copy_cb(opaque: *mut libc::c_void,
                            pos: raw::rs_long_t,
                            len: *mut libc::size_t,
                            buf: *mut *mut libc::c_void)
                            -> raw::rs_result {
    let input = unsafe {
        let h: *mut StreamHolder = mem::transmute(opaque);
        (*h).as_stream()
    };
    let output = unsafe {
        let buf: *mut u8 = mem::transmute(*buf);
        slice::from_raw_parts_mut(buf, *len)
    };
    try_or_rs_error!(input.seek(io::SeekFrom::Start(pos as u64)));
    try_or_rs_error!(input.read(output));
    raw::RS_DONE
}


fn io_err<E>(kind: io::ErrorKind, e: E) -> Error
    where E: Into<Box<error::Error + Send + Sync>>
{
    Error::Io(io::Error::new(kind, e))
}


#[cfg(test)]
mod test {
    use super::*;
    use std::io::{Cursor, Read};

    const DATA: &'static str = "this is a string to be tested";
    const DATA2: &'static str = "this is another string to be tested";

    // generated with `rdiff signature -b 10 -S 5 data data.sig`
    fn data_signature() -> Vec<u8> {
        vec![0x72, 0x73, 0x01, 0x36, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x05, 0x1b, 0x21,
             0x04, 0x8b, 0xad, 0x3c, 0xbd, 0x19, 0x09, 0x1d, 0x1b, 0x04, 0xf0, 0x9d, 0x1f, 0x64,
             0x31, 0xde, 0x15, 0xf4, 0x04, 0x87, 0x60, 0x96, 0x19, 0x50, 0x39]
    }

    // generated with `rdiff delta data.sig data2 data2.delta`
    fn data2_delta() -> Vec<u8> {
        vec![0x72, 0x73, 0x02, 0x36, 0x41, 0x10, 0x74, 0x68, 0x69, 0x73, 0x20, 0x69, 0x73, 0x20,
             0x61, 0x6e, 0x6f, 0x74, 0x68, 0x65, 0x72, 0x20, 0x45, 0x0a, 0x13, 0x00]
    }


    #[test]
    fn signature() {
        let cursor = Cursor::new(DATA);
        let mut sig = Signature::new(cursor, 10, 5, SignatureType::MD4).unwrap();
        let mut signature = Vec::new();
        let read = sig.read_to_end(&mut signature).unwrap();
        assert_eq!(read, signature.len());
        assert_eq!(signature, data_signature());
    }

    #[test]
    fn delta() {
        let sig = data_signature();
        let sig = Cursor::new(sig);
        let input = Cursor::new(DATA2);
        let mut job = Delta::new(input, sig).unwrap();
        let mut delta = Vec::new();
        let read = job.read_to_end(&mut delta).unwrap();
        assert_eq!(read, delta.len());
        assert_eq!(delta, data2_delta());
    }

    #[test]
    fn patch() {
        let base = Cursor::new(DATA);
        let delta = data2_delta();
        let delta = Cursor::new(delta);
        let mut patch = Patch::new(base, delta).unwrap();
        let mut computed_new = String::new();
        patch.read_to_string(&mut computed_new).unwrap();
        assert_eq!(computed_new, DATA2);
    }

    #[test]
    fn integration() {
        let base = Cursor::new(DATA);
        let new = Cursor::new(DATA2);
        let sig = Signature::new(base, 10, 5, SignatureType::MD4).unwrap();
        let delta = Delta::new(new, sig).unwrap();
        let base = Cursor::new(DATA);
        let mut patch = Patch::new(base, delta).unwrap();
        let mut computed_new = String::new();
        patch.read_to_string(&mut computed_new).unwrap();
        assert_eq!(computed_new, DATA2);
    }
}
