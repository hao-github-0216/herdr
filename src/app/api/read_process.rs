//! Bounded pipe I/O without auxiliary threads. Timeout kills and reaps the child.
use std::{io, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) struct ProcessOutput {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

pub(super) async fn run_bounded(
    mut command: std::process::Command,
    limit: usize,
    timeout: Duration,
    allow_empty_exit_one: bool,
    input: Option<Vec<u8>>,
) -> io::Result<ProcessOutput> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing stderr"))?;
    let stdin = child.stdin.take();
    async fn read_pipe(
        reader: impl tokio::io::AsyncRead + Unpin,
        limit: usize,
    ) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        reader.take(limit as u64).read_to_end(&mut bytes).await?;
        Ok(bytes)
    }
    let read = async {
        let write = async move {
            if let (Some(mut stdin), Some(input)) = (stdin, input) {
                stdin.write_all(&input).await?;
                stdin.shutdown().await?;
            }
            Ok::<(), io::Error>(())
        };
        let (bytes, errors, status, written) = tokio::join!(
            read_pipe(stdout, limit + 1),
            read_pipe(stderr, 4097),
            child.wait(),
            write
        );
        let mut bytes = bytes?;
        let errors = errors?;
        let status = status?;
        let truncated = bytes.len() > limit;
        bytes.truncate(limit);
        let no_match = allow_empty_exit_one
            && status.code() == Some(1)
            && bytes.is_empty()
            && errors.is_empty();
        if !status.success() && !truncated && !no_match {
            return Err(io::Error::other(
                String::from_utf8_lossy(&errors).into_owned(),
            ));
        }
        written?;
        Ok(ProcessOutput { bytes, truncated })
    };
    match tokio::time::timeout(timeout, read).await {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "read subprocess timed out",
            ))
        }
    }
}
