use crate::event::Source;
use crate::sys::windows::{Event, Overlapped};
use crate::Registry;

use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read, Write};
use std::mem;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, RawHandle};
use std::slice;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::{Arc, Mutex};

use crate::{Interest, Token};
use miow::pipe;
use winapi::shared::winerror::{ERROR_BROKEN_PIPE, ERROR_PIPE_LISTENING};
use winapi::um::ioapiset::CancelIoEx;

/// Non-blocking windows named pipe.
///
/// This structure internally contains a `HANDLE` which represents the named
/// pipe, and also maintains state associated with the mio event loop and active
/// I/O operations that have been scheduled to translate IOCP to a readiness
/// model.
///
/// Note, IOCP is a *completion* based model whereas mio is a *readiness* based
/// model. To bridge this, `NamedPipe` performs internal buffering. Writes are
/// written to an internal buffer and the buffer is submitted to IOCP. IOCP
/// reads are submitted using internal buffers and `NamedPipe::read` reads from
/// this internal buffer.
///
/// # Trait implementations
///
/// The `Read` and `Write` traits are implemented for `NamedPipe` and for
/// `&NamedPipe`. This represents that a named pipe can be concurrently read and
/// written to and also can be read and written to at all. Typically a named
/// pipe needs to be connected to a client before it can be read or written,
/// however.
///
/// Note that for I/O operations on a named pipe to succeed then the named pipe
/// needs to be associated with an event loop. Until this happens all I/O
/// operations will return a "would block" error.
///
/// # Managing connections
///
/// The `NamedPipe` type supports a `connect` method to connect to a client and
/// a `disconnect` method to disconnect from that client. These two methods only
/// work once a named pipe is associated with an event loop.
///
/// The `connect` method will succeed asynchronously and a completion can be
/// detected once the object receives a writable notification.
///
/// # Named pipe clients
///
/// Currently to create a client of a named pipe server then you can use the
/// `OpenOptions` type in the standard library to create a `File` that connects
/// to a named pipe. Afterwards you can use the `into_raw_handle` method coupled
/// with the `NamedPipe::from_raw_handle` method to convert that to a named pipe
/// that can operate asynchronously. Don't forget to pass the
/// `FILE_FLAG_OVERLAPPED` flag when opening the `File`.
pub struct NamedPipe {
    inner: Arc<Inner>,
}

/// # Notes
///
/// The memory layout of this structure must be fixed as the
/// `ptr_from_*_overlapped` methods depend on it, see the `ptr_from` test.
#[repr(C)]
struct Inner {
    // NOTE: careful modifying the order of these three fields, the `ptr_from_*`
    // methods depend on the layout!
    connect: Overlapped,
    read: Overlapped,
    write: Overlapped,
    // END NOTE.
    handle: pipe::NamedPipe,
    connecting: AtomicBool,
    io: Mutex<Io>,
    pool: Mutex<BufferPool>,
}

#[test]
fn ptr_from() {
    use std::mem::ManuallyDrop;
    use std::ptr;

    let pipe = unsafe { ManuallyDrop::new(NamedPipe::from_raw_handle(ptr::null_mut())) };
    let inner: &Inner = &pipe.inner;
    assert_eq!(
        inner as *const Inner,
        unsafe { Inner::ptr_from_conn_overlapped(&inner.connect as *const _ as *mut OVERLAPPED) },
        "`ptr_from_conn_overlapped` incorrect"
    );
    assert_eq!(
        inner as *const Inner,
        unsafe { Inner::ptr_from_read_overlapped(&inner.read as *const _ as *mut OVERLAPPED) },
        "`ptr_from_read_overlapped` incorrect"
    );
    assert_eq!(
        inner as *const Inner,
        unsafe { Inner::ptr_from_write_overlapped(&inner.write as *const _ as *mut OVERLAPPED) },
        "`ptr_from_write_overlapped` incorrect"
    );
}

struct Io {
    // Token used to identify events
    token: Option<Token>,
    read: State,
    write: State,
    connect_error: Option<io::Error>,
}

#[derive(Debug)]
enum State {
    None,
    Pending(Vec<u8>, usize),
    Ok(Vec<u8>, usize),
    Err(io::Error),
}

fn would_block() -> io::Error {
    io::ErrorKind::WouldBlock.into()
}

impl NamedPipe {
    /// Creates a new named pipe at the specified `addr` given a "reasonable
    /// set" of initial configuration options.
    pub fn new<A: AsRef<OsStr>>(addr: A) -> io::Result<NamedPipe> {
        let pipe = pipe::NamedPipe::new(addr)?;
        // Safety: nothing actually unsafe about this. The trait fn includes
        // `unsafe`.
        Ok(unsafe { NamedPipe::from_raw_handle(pipe.into_raw_handle()) })
    }

    /// Attempts to call `ConnectNamedPipe`, if possible.
    ///
    /// This function will attempt to connect this pipe to a client in an
    /// asynchronous fashion. If the function immediately establishes a
    /// connection to a client then `Ok(())` is returned. Otherwise if a
    /// connection attempt was issued and is now in progress then a "would
    /// block" error is returned.
    ///
    /// When the connection is finished then this object will be flagged as
    /// being ready for a write, or otherwise in the writable state.
    ///
    /// # Errors
    ///
    /// This function will return a "would block" error if the pipe has not yet
    /// been registered with an event loop, if the connection operation has
    /// previously been issued but has not yet completed, or if the connect
    /// itself was issued and didn't finish immediately.
    ///
    /// Normal I/O errors from the call to `ConnectNamedPipe` are returned
    /// immediately.
    pub fn connect(&self) -> io::Result<()> {
        // "Acquire the connecting lock" or otherwise just make sure we're the
        // only operation that's using the `connect` overlapped instance.
        if self.inner.connecting.swap(true, SeqCst) {
            return Err(would_block());
        }

        // Now that we've flagged ourselves in the connecting state, issue the
        // connection attempt. Afterwards interpret the return value and set
        // internal state accordingly.
        let res = unsafe {
            let overlapped = self.inner.connect.as_ptr() as *mut _;
            self.inner.handle.connect_overlapped(overlapped)
        };

        match res {
            // The connection operation finished immediately, so let's schedule
            // reads/writes and such.
            Ok(true) => {
                self.inner.connecting.store(false, SeqCst);
                Inner::post_register(&self.inner, None);
                Ok(())
            }

            // If the overlapped operation was successful and didn't finish
            // immediately then we forget a copy of the arc we hold
            // internally. This ensures that when the completion status comes
            // in for the I/O operation finishing it'll have a reference
            // associated with it and our data will still be valid. The
            // `connect_done` function will "reify" this forgotten pointer to
            // drop the refcount on the other side.
            Ok(false) => {
                mem::forget(self.inner.clone());
                Err(would_block())
            }

            Err(e) => {
                self.inner.connecting.store(false, SeqCst);
                Err(e)
            }
        }
    }

    /// Takes any internal error that has happened after the last I/O operation
    /// which hasn't been retrieved yet.
    ///
    /// This is particularly useful when detecting failed attempts to `connect`.
    /// After a completed `connect` flags this pipe as writable then callers
    /// must invoke this method to determine whether the connection actually
    /// succeeded. If this function returns `None` then a client is connected,
    /// otherwise it returns an error of what happened and a client shouldn't be
    /// connected.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        Ok(self.inner.io.lock().unwrap().connect_error.take())
    }

    /// Disconnects this named pipe from a connected client.
    ///
    /// This function will disconnect the pipe from a connected client, if any,
    /// transitively calling the `DisconnectNamedPipe` function.
    ///
    /// After a `disconnect` is issued, then a `connect` may be called again to
    /// connect to another client.
    pub fn disconnect(&self) -> io::Result<()> {
        self.inner.handle.disconnect()
    }
}

impl FromRawHandle for NamedPipe {
    unsafe fn from_raw_handle(_handle: RawHandle) -> NamedPipe {
        unimplemented!()
    }
}

impl Read for NamedPipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        <&NamedPipe as Read>::read(&mut &*self, buf)
    }
}

impl Write for NamedPipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        <&NamedPipe as Write>::write(&mut &*self, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        <&NamedPipe as Write>::flush(&mut &*self)
    }
}

impl<'a> Read for &'a NamedPipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut state = self.inner.io.lock().unwrap();

        if state.token.is_none() {
            return Err(would_block());
        }

        match mem::replace(&mut state.read, State::None) {
            // In theory not possible with `token` checked above,
            // but return would block for now.
            State::None => Err(would_block()),

            // A read is in flight, still waiting for it to finish
            State::Pending(buf, amt) => {
                state.read = State::Pending(buf, amt);
                Err(would_block())
            }

            // We previously read something into `data`, try to copy out some
            // data. If we copy out all the data schedule a new read and
            // otherwise store the buffer to get read later.
            State::Ok(data, cur) => {
                let n = {
                    let mut remaining = &data[cur..];
                    remaining.read(buf)?
                };
                let next = cur + n;
                if next != data.len() {
                    state.read = State::Ok(data, next);
                } else {
                    self.inner.put_buffer(data);
                    Inner::schedule_read(&self.inner, &mut state, None);
                }
                Ok(n)
            }

            // Looks like an in-flight read hit an error, return that here while
            // we schedule a new one.
            State::Err(e) => {
                Inner::schedule_read(&self.inner, &mut state, None);
                if e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                    Ok(0)
                } else {
                    Err(e)
                }
            }
        }
    }
}

impl<'a> Write for &'a NamedPipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Make sure there's no writes pending
        let mut io = self.inner.io.lock().unwrap();

        if io.token.is_none() {
            return Err(would_block());
        }

        match io.write {
            State::None => {}
            State::Err(_) => match mem::replace(&mut io.write, State::None) {
                State::Err(e) => return Err(e),
                // `io` is locked, so this branch is unreachable
                _ => unreachable!(),
            },
            // any other state should be handled in `write_done`
            _ => {
                return Err(would_block());
            }
        }

        // Move `buf` onto the heap and fire off the write
        let mut owned_buf = self.inner.get_buffer();
        owned_buf.extend(buf);
        match Inner::maybe_schedule_write(&self.inner, owned_buf, 0, &mut io)? {
            // Some bytes are written immediately
            Some(n) => Ok(n),
            // Write operation is anqueued for whole buffer
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Source for NamedPipe {
    fn register(&mut self, _registry: &Registry, _token: Token, _: Interest) -> io::Result<()> {
        unimplemented!()
    }

    fn reregister(&mut self, registry: &Registry, token: Token, _: Interest) -> io::Result<()> {
        let mut io = self.inner.io.lock().unwrap();

        io.check_association(registry, true)?;

        io.token = Some(token);
        drop(io);

        Inner::post_register(&self.inner, None);

        Ok(())
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        let mut io = self.inner.io.lock().unwrap();

        io.check_association(registry, true)?;

        if io.token.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "I/O source not registered with `Registry`",
            ));
        }

        io.token = None;
        Ok(())
    }
}

impl AsRawHandle for NamedPipe {
    fn as_raw_handle(&self) -> RawHandle {
        self.inner.handle.as_raw_handle()
    }
}

impl fmt::Debug for NamedPipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.handle.fmt(f)
    }
}

impl Drop for NamedPipe {
    fn drop(&mut self) {
        // Cancel pending reads/connects, but don't cancel writes to ensure that
        // everything is flushed out.
        unsafe {
            if self.inner.connecting.load(SeqCst) {
                drop(cancel(&self.inner.handle, &self.inner.connect));
            }

            let io = self.inner.io.lock().unwrap();

            match io.read {
                State::Pending(..) => {
                    drop(cancel(&self.inner.handle, &self.inner.read));
                }
                _ => {}
            }
        }
    }
}

impl Inner {
    /// Schedules a read to happen in the background, executing an overlapped
    /// operation.
    ///
    /// This function returns `true` if a normal error happens or if the read
    /// is scheduled in the background. If the pipe is no longer connected
    /// (ERROR_PIPE_LISTENING) then `false` is returned and no read is
    /// scheduled.
    fn schedule_read(me: &Arc<Inner>, io: &mut Io, events: Option<&mut Vec<Event>>) -> bool {
        // Check to see if a read is already scheduled/completed
        match io.read {
            State::None => {}
            _ => return true,
        }

        // Allocate a buffer and schedule the read.
        let mut buf = me.get_buffer();
        let e = unsafe {
            let overlapped = me.read.as_ptr() as *mut _;
            let slice = slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.capacity());
            me.handle.read_overlapped(slice, overlapped)
        };

        match e {
            // See `NamedPipe::connect` above for the rationale behind `forget`
            Ok(_) => {
                io.read = State::Pending(buf, 0); // 0 is ignored on read side
                mem::forget(me.clone());
                true
            }

            // If ERROR_PIPE_LISTENING happens then it's not a real read error,
            // we just need to wait for a connect.
            Err(ref e) if e.raw_os_error() == Some(ERROR_PIPE_LISTENING as i32) => false,

            // If some other error happened, though, we're now readable to give
            // out the error.
            Err(e) => {
                io.read = State::Err(e);
                io.notify_readable(events);
                true
            }
        }
    }

    /// Maybe schedules overlapped write operation.
    ///
    /// * `None` means that overlapped operation was enqueued
    /// * `Some(n)` means that `n` bytes was immediately written.
    ///   Note, that `write_done` will fire anyway to clean up the state.
    fn maybe_schedule_write(
        me: &Arc<Inner>,
        buf: Vec<u8>,
        pos: usize,
        io: &mut Io,
    ) -> io::Result<Option<usize>> {
        // Very similar to `schedule_read` above, just done for the write half.
        let e = unsafe {
            let overlapped = me.write.as_ptr() as *mut _;
            me.handle.write_overlapped(&buf[pos..], overlapped)
        };

        // See `connect` above for the rationale behind `forget`
        match e {
            // `n` bytes are written immediately
            Ok(Some(n)) => {
                io.write = State::Ok(buf, pos);
                mem::forget(me.clone());
                Ok(Some(n))
            }
            // write operation is enqueued
            Ok(None) => {
                io.write = State::Pending(buf, pos);
                mem::forget(me.clone());
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn post_register(me: &Arc<Inner>, mut events: Option<&mut Vec<Event>>) {
        let mut io = me.io.lock().unwrap();
        if Inner::schedule_read(&me, &mut io, events.as_mut().map(|ptr| &mut **ptr)) {
            if let State::None = io.write {
                io.notify_writable(events);
            }
        }
    }

    fn get_buffer(&self) -> Vec<u8> {
        self.pool.lock().unwrap().get(4 * 1024)
    }

    fn put_buffer(&self, buf: Vec<u8>) {
        self.pool.lock().unwrap().put(buf)
    }
}

unsafe fn cancel<T: AsRawHandle>(handle: &T, overlapped: &Overlapped) -> io::Result<()> {
    let ret = CancelIoEx(handle.as_raw_handle(), overlapped.as_ptr() as *mut _);
    // `CancelIoEx` returns 0 on error:
    // https://docs.microsoft.com/en-us/windows/win32/fileio/cancelioex-func
    if ret == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl Io {
    fn check_association(&self, _registry: &Registry, _required: bool) -> io::Result<()> {
        unimplemented!()
    }

    fn notify_readable(&self, _events: Option<&mut Vec<Event>>) {
        unimplemented!()
    }

    fn notify_writable(&self, _events: Option<&mut Vec<Event>>) {
        unimplemented!()
    }
}

struct BufferPool {
    pool: Vec<Vec<u8>>,
}

impl BufferPool {
    fn get(&mut self, default_cap: usize) -> Vec<u8> {
        self.pool
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(default_cap))
    }

    fn put(&mut self, mut buf: Vec<u8>) {
        if self.pool.len() < self.pool.capacity() {
            unsafe {
                buf.set_len(0);
            }
            self.pool.push(buf);
        }
    }
}
