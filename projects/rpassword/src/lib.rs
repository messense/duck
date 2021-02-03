//! This library makes it easy to read passwords in a console application on all platforms, Unix and
//! Windows alike.
//!
//! Here's how you can read a password from stdin:
//! ```no_run
//! let password = rpassword::read_password().unwrap();
//! println!("Your password is {}", password);
//! ```
//!
//! If you need more control over the source of the input, you can use `read_password_from_stdin_lock`,
//! which hides the input from the terminal, all the while using the StdinLock you pass it:
//! ```no_run
//! let stdin = std::io::stdin();
//! let password = rpassword::read_password_from_stdin_lock(&mut stdin.lock()).unwrap();
//! println!("Your password is {}", password);
//! ```
//!
//! In more advanced scenarios, you'll want to read from the TTY instead of stdin:
//! ```no_run
//! let password = rpassword::read_password_from_tty().unwrap();
//! println!("Your password is {}", password);
//! ```
//!
//! Finally, in unit tests, you might want to pass a `Cursor`, which implements `BufRead`. In that
//! case, you can use `read_password_from_bufread`:
//! ```
//! use std::io::Cursor;
//!
//! let mut mock_input = Cursor::new("my-password\n".as_bytes().to_owned());
//! let password = rpassword::read_password_from_bufread(&mut mock_input).unwrap();
//! println!("Your password is {}", password);
//! ```

#[cfg(unix)]
extern crate libc;

#[cfg(windows)]
extern crate winapi;

extern crate rutil;

use rutil::fix_new_line::fix_new_line;
use rutil::safe_string::SafeString;
use std::io::BufRead;

#[cfg(unix)]
mod unix {
    use libc::{c_int, tcsetattr, termios, ECHO, ECHONL, STDIN_FILENO, TCSANOW};
    use rutil::stdin_is_tty::stdin_is_tty;
    use std::io::{self, BufRead, StdinLock};
    use std::mem;
    use std::os::unix::io::AsRawFd;

    struct HiddenInput {
        fd: i32,
        term_orig: termios,
    }

    impl HiddenInput {
        fn new(fd: i32) -> io::Result<HiddenInput> {
            // Make two copies of the terminal settings. The first one will be modified
            // and the second one will act as a backup for when we want to set the
            // terminal back to its original state.
            let mut term = safe_tcgetattr(fd)?;
            let term_orig = safe_tcgetattr(fd)?;

            // Hide the password. This is what makes this function useful.
            term.c_lflag &= !ECHO;

            // But don't hide the NL character when the user hits ENTER.
            term.c_lflag |= ECHONL;

            // Save the settings for now.
            io_result(unsafe { tcsetattr(fd, TCSANOW, &term) })?;

            Ok(HiddenInput { fd, term_orig })
        }
    }

    impl Drop for HiddenInput {
        fn drop(&mut self) {
            // Set the the mode back to normal
            unsafe {
                tcsetattr(self.fd, TCSANOW, &self.term_orig);
            }
        }
    }

    /// Turns a C function return into an IO Result
    fn io_result(ret: c_int) -> ::std::io::Result<()> {
        match ret {
            0 => Ok(()),
            _ => Err(::std::io::Error::last_os_error()),
        }
    }

    fn safe_tcgetattr(fd: c_int) -> ::std::io::Result<termios> {
        let mut term = mem::MaybeUninit::<::unix::termios>::uninit();
        io_result(unsafe { ::libc::tcgetattr(fd, term.as_mut_ptr()) })?;
        Ok(unsafe { term.assume_init() })
    }

    /// Reads a password from the TTY
    pub fn read_password_from_tty() -> ::std::io::Result<String> {
        let tty = ::std::fs::File::open("/dev/tty")?;
        let fd = tty.as_raw_fd();
        let mut source = io::BufReader::new(tty);

        read_password_from_fd(&mut source, fd)
    }

    /// Reads a password from an existing StdinLock
    pub fn read_password_from_stdin_lock(reader: &mut StdinLock) -> ::std::io::Result<String> {
        if stdin_is_tty() {
            read_password_from_fd(reader, STDIN_FILENO)
        } else {
            ::read_password_from_bufread(reader)
        }
    }

    /// Reads a password from a given file descriptor
    fn read_password_from_fd(reader: &mut impl BufRead, fd: i32) -> ::std::io::Result<String> {
        let mut password = super::SafeString::new();

        let hidden_input = HiddenInput::new(fd)?;

        reader.read_line(&mut password)?;

        std::mem::drop(hidden_input);

        super::fix_new_line(password.into_inner())
    }
}

#[cfg(windows)]
mod windows {
    use std::io::{self, BufReader};
    use std::io::{BufRead, StdinLock};
    use std::os::windows::io::FromRawHandle;
    use winapi::shared::minwindef::LPDWORD;
    use winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
    use winapi::um::fileapi::{CreateFileA, GetFileType, OPEN_EXISTING};
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::processenv::GetStdHandle;
    use winapi::um::winbase::{FILE_TYPE_PIPE, STD_INPUT_HANDLE};
    use winapi::um::wincon::{ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT};
    use winapi::um::winnt::{
        FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ, GENERIC_WRITE, HANDLE,
    };

    struct HiddenInput {
        mode: u32,
        handle: HANDLE,
    }

    impl HiddenInput {
        fn new(handle: HANDLE) -> io::Result<HiddenInput> {
            let mut mode = 0;

            // Get the old mode so we can reset back to it when we are done
            if unsafe { GetConsoleMode(handle, &mut mode as LPDWORD) } == 0 {
                return Err(::std::io::Error::last_os_error());
            }

            // We want to be able to read line by line, and we still want backspace to work
            let new_mode_flags = ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT;
            if unsafe { SetConsoleMode(handle, new_mode_flags) } == 0 {
                return Err(::std::io::Error::last_os_error());
            }

            Ok(HiddenInput { mode, handle })
        }
    }

    impl Drop for HiddenInput {
        fn drop(&mut self) {
            // Set the the mode back to normal
            unsafe {
                SetConsoleMode(self.handle, self.mode);
            }
        }
    }

    /// Reads a password from the TTY
    pub fn read_password_from_tty() -> ::std::io::Result<String> {
        let handle = unsafe {
            CreateFileA(
                b"CONIN$\x00".as_ptr() as *const i8,
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            return Err(::std::io::Error::last_os_error());
        }

        let mut stream = BufReader::new(unsafe { ::std::fs::File::from_raw_handle(handle) });
        read_password_from_handle(&mut stream, handle)
    }

    /// Reads a password from an existing StdinLock
    pub fn read_password_from_stdin_lock(reader: &mut StdinLock) -> ::std::io::Result<String> {
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if handle == INVALID_HANDLE_VALUE {
            return Err(::std::io::Error::last_os_error());
        }

        if unsafe { GetFileType(handle) } == FILE_TYPE_PIPE {
            ::read_password_from_bufread(reader)
        } else {
            read_password_from_handle(reader, handle)
        }
    }

    /// Reads a password from a given file handle
    fn read_password_from_handle(reader: &mut impl BufRead, handle: HANDLE) -> io::Result<String> {
        let mut password = super::SafeString::new();

        let hidden_input = HiddenInput::new(handle)?;

        reader.read_line(&mut password)?;

        // Newline for windows which otherwise prints on the same line.
        println!();

        std::mem::drop(hidden_input);

        super::fix_new_line(password.into_inner())
    }
}

#[cfg(unix)]
pub use unix::{read_password_from_stdin_lock, read_password_from_tty};
#[cfg(windows)]
pub use windows::{read_password_from_stdin_lock, read_password_from_tty};

/// Reads a password from stdin
pub fn read_password() -> ::std::io::Result<String> {
    read_password_from_stdin_lock(&mut std::io::stdin().lock())
}

/// Reads a password from anything that implements BufRead
pub fn read_password_from_bufread(source: &mut impl BufRead) -> ::std::io::Result<String> {
    let mut password = SafeString::new();
    source.read_line(&mut password)?;

    rutil::fix_new_line::fix_new_line(password.into_inner())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    fn mock_input_crlf() -> Cursor<&'static [u8]> {
        Cursor::new(&b"A mocked response.\r\nAnother mocked response.\r\n"[..])
    }

    fn mock_input_lf() -> Cursor<&'static [u8]> {
        Cursor::new(&b"A mocked response.\nAnother mocked response.\n"[..])
    }

    #[test]
    fn can_read_from_redirected_input_many_times() {
        let mut reader_crlf = mock_input_crlf();

        let response = ::read_password_from_bufread(&mut reader_crlf).unwrap();
        assert_eq!(response, "A mocked response.");
        let response = ::read_password_from_bufread(&mut reader_crlf).unwrap();
        assert_eq!(response, "Another mocked response.");

        let mut reader_lf = mock_input_lf();
        let response = ::read_password_from_bufread(&mut reader_lf).unwrap();
        assert_eq!(response, "A mocked response.");
        let response = ::read_password_from_bufread(&mut reader_lf).unwrap();
        assert_eq!(response, "Another mocked response.");
    }
}
