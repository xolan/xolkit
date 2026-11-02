//! The app's connection to the daemon: one request at a time, without an async runtime.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use interprocess::local_socket::Stream;
use xolkit_core::{Action, Env};

use crate::ipc;
use crate::protocol::{Hello, Request, Response, Status};

pub struct Client {
    env: Env,
    exe: PathBuf,
    connection: Option<Stream>,
}

impl Client {
    /// A client for the daemon of `env`, which `exe daemon` starts when it isn't running.
    /// Connects on the first request.
    pub fn new(env: Env, exe: &Path) -> Client {
        Client {
            env,
            exe: exe.to_path_buf(),
            connection: None,
        }
    }

    fn connect(&mut self) -> Result<&Stream> {
        if self.connection.is_none() {
            let mut stream = ipc::connect_or_start_blocking(&self.env, &self.exe)?;
            let hello = serde_json::to_string(&Hello::Control)? + "\n";
            stream.write_all(hello.as_bytes())?;
            self.connection = Some(stream);
        }
        Ok(self.connection.as_ref().expect("just connected"))
    }

    fn send(&mut self, request: &Request) -> Result<Response> {
        let line = serde_json::to_string(request)? + "\n";
        let mut stream = self.connect()?;
        stream.write_all(line.as_bytes())?;
        stream.flush()?;
        // One answer per request, so a reader per request loses nothing it buffered.
        let mut answer = String::new();
        if BufReader::new(stream).read_line(&mut answer)? == 0 {
            bail!("the daemon closed the connection");
        }
        serde_json::from_str(&answer).context("the daemon answered something unexpected")
    }

    /// Sends a request, connecting again if the daemon went away since the last one. A change is
    /// never sent twice: the connection is checked with a status request first, as a change
    /// that got through but lost its answer would otherwise run again.
    pub fn request(&mut self, request: &Request) -> Result<Response> {
        let read_only = matches!(request, Request::Status);
        if !read_only && self.connection.is_some() && self.send(&Request::Status).is_err() {
            self.connection = None;
        }
        let had_connection = self.connection.is_some();
        match self.send(request) {
            Ok(response) => Ok(response),
            Err(_) if read_only && had_connection => {
                self.connection = None;
                self.send(request)
            }
            Err(e) => {
                self.connection = None;
                Err(e)
            }
        }
    }

    /// Sends a request that answers `Ok` or an error.
    pub fn call(&mut self, request: &Request) -> Result<()> {
        match self.request(request)? {
            Response::Ok => Ok(()),
            Response::Error { message } => bail!(message),
            other => bail!("unexpected answer: {other:?}"),
        }
    }

    pub fn execute(&mut self, actions: Vec<Action>) -> Result<()> {
        self.call(&Request::Execute { actions })
    }

    pub fn status(&mut self) -> Result<Status> {
        match self.request(&Request::Status)? {
            Response::Status(status) => Ok(*status),
            Response::Error { message } => bail!(message),
            other => bail!("unexpected answer: {other:?}"),
        }
    }

    /// Drops the connection, so the daemon can count the app as gone.
    pub fn disconnect(&mut self) {
        self.connection = None;
    }
}
