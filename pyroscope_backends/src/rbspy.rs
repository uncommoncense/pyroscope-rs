use super::error::{BackendError, Result};
use super::types::{Backend, State};

use rbspy::{sampler::Sampler, ui::output::Outputter, StackFrame, StackTrace};

use std::collections::HashMap;
use std::io::Write;
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender};

/// Rbspy Configuration
#[derive(Debug)]
pub struct RbspyConfig {
    /// Process to monitor
    pid: Option<i32>,
    /// Sampling rate
    sample_rate: u32,
    /// Lock Process while sampling
    lock_process: bool,
    /// Profiling duration. None for infinite.
    time_limit: Option<core::time::Duration>,
    /// Include subprocesses
    with_subprocesses: bool,
}

impl Default for RbspyConfig {
    fn default() -> Self {
        RbspyConfig {
            pid: None,
            sample_rate: 100,
            lock_process: false,
            time_limit: None,
            with_subprocesses: false,
        }
    }
}

impl RbspyConfig {
    /// Create a new RbspyConfig
    pub fn new(pid: i32) -> Self {
        RbspyConfig {
            pid: Some(pid),
            ..Default::default()
        }
    }

    pub fn sample_rate(self, sample_rate: u32) -> Self {
        RbspyConfig {
            sample_rate,
            ..self
        }
    }

    pub fn lock_process(self, lock_process: bool) -> Self {
        RbspyConfig {
            lock_process,
            ..self
        }
    }

    pub fn time_limit(self, time_limit: Option<core::time::Duration>) -> Self {
        RbspyConfig { time_limit, ..self }
    }

    pub fn with_subprocesses(self, with_subprocesses: bool) -> Self {
        RbspyConfig {
            with_subprocesses,
            ..self
        }
    }
}

/// Rbspy Backend
#[derive(Default)]
pub struct Rbspy {
    /// Rbspy State
    state: State,
    /// Rbspy Configuration
    config: RbspyConfig,
    /// Rbspy Sampler
    sampler: Option<Sampler>,
    /// StackTrace Receiver
    stack_receiver: Option<Receiver<StackTrace>>,
    /// Error Receiver
    error_receiver: Option<Receiver<std::result::Result<(), anyhow::Error>>>,
}

impl std::fmt::Debug for Rbspy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rbspy Backend")
    }
}

impl Rbspy {
    pub fn new(config: RbspyConfig) -> Self {
        Rbspy {
            sampler: None,
            stack_receiver: None,
            error_receiver: None,
            state: State::Uninitialized,
            config,
        }
    }
}

impl Backend for Rbspy {
    fn get_state(&self) -> State {
        self.state
    }

    fn spy_name(&self) -> Result<String> {
        Ok("rbspy".to_string())
    }

    fn sample_rate(&self) -> Result<u32> {
        Ok(self.config.sample_rate)
    }

    fn initialize(&mut self) -> Result<()> {
        // Check if Backend is Uninitialized
        if self.state != State::Uninitialized {
            return Err(BackendError::new("Rbspy: Backend is already Initialized"));
        }

        // Check if a process ID is set
        if self.config.pid.is_none() {
            return Err(BackendError::new("Rbspy: No Process ID Specified"));
        }

        // Create Sampler
        self.sampler = Some(Sampler::new(
            self.config.pid.unwrap(), // unwrap is safe because of check above
            self.config.sample_rate,
            self.config.lock_process,
            self.config.time_limit,
            self.config.with_subprocesses,
        ));

        // Set State to Ready
        self.state = State::Ready;

        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        // Check if Backend is Ready
        if self.state != State::Ready {
            return Err(BackendError::new("Rbspy: Backend is not Ready"));
        }

        // Channel for Errors generated by the RubySpy Sampler
        let (error_sender, error_receiver): (
            Sender<std::result::Result<(), anyhow::Error>>,
            Receiver<std::result::Result<(), anyhow::Error>>,
        ) = channel();

        // This is provides enough space for 100 threads.
        // It might be a better idea to figure out how many threads are running and determine the
        // size of the channel based on that.
        let queue_size: usize = self.config.sample_rate as usize * 10 * 100;

        // Channel for StackTraces generated by the RubySpy Sampler
        let (stack_sender, stack_receiver): (SyncSender<StackTrace>, Receiver<StackTrace>) =
            sync_channel(queue_size);

        // Set Error and Stack Receivers
        self.stack_receiver = Some(stack_receiver);
        self.error_receiver = Some(error_receiver);

        // Get the Sampler
        let sampler = self
            .sampler
            .as_ref()
            .ok_or_else(|| BackendError::new("Rbspy: Sampler is not set"))?;

        // Start the Sampler
        sampler.start(stack_sender, error_sender)?;

        // Set State to Running
        self.state = State::Running;

        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        // Check if Backend is Running
        if self.state != State::Running {
            return Err(BackendError::new("Rbspy: Backend is not Running"));
        }

        // Stop Sampler
        self.sampler
            .as_ref()
            .ok_or_else(|| BackendError::new("Rbspy: Sampler is not set"))?
            .stop();

        // Set State to Running
        self.state = State::Ready;

        Ok(())
    }

    fn report(&mut self) -> Result<Vec<u8>> {
        // Check if Backend is Running
        if self.state != State::Running {
            return Err(BackendError::new("Rbspy: Backend is not Running"));
        }

        // Send Errors to Log
        let errors = self
            .error_receiver
            .as_ref()
            .ok_or_else(|| BackendError::new("Rbspy: error receiver is not set"))?
            .try_iter();
        for error in errors {
            match error {
                Ok(_) => {}
                Err(e) => {
                    log::error!("Rbspy: Error in Sampler: {}", e);
                }
            }
        }

        // Collect the StackTrace from the receiver
        let stack_trace = self
            .stack_receiver
            .as_ref()
            .ok_or_else(|| BackendError::new("Rbspy: StackTrace receiver is not set"))?
            .try_iter();

        // Create a new OutputFormat (collapsed). This is an object provided by rbspy.
        // The argument should be ignored.
        let mut outputter = RbspyFormatter::default();

        // Iterate over the StackTrace
        for trace in stack_trace {
            // Write the StackTrace to the OutputFormat
            outputter.record(&trace)?;
        }

        // buffer to store the output
        let mut buffer: Vec<u8> = Vec::new();

        // Create a new writer
        let mut writer = RbspyWriter::new(&mut buffer);

        // Push the outputter into our writer
        outputter.complete(&mut writer)?;

        // Flush the Writer
        writer.flush()?;

        // Return the writer's buffer
        Ok(buffer)
    }
}

/// Rbspy Formatter
#[derive(Default)]
pub struct RbspyFormatter(HashMap<String, usize>);

impl Outputter for RbspyFormatter {
    fn record(&mut self, stack: &StackTrace) -> std::result::Result<(), anyhow::Error> {
        let frame = stack
            .iter()
            .rev()
            .map(|frame| format!("{}", StackFrameFormatter(frame)))
            .collect::<Vec<String>>()
            .join(";");

        *self.0.entry(frame).or_insert(0) += 1;

        Ok(())
    }
    fn complete(&mut self, writer: &mut dyn Write) -> std::result::Result<(), anyhow::Error> {
        if self.0.is_empty() {
            log::info!("Rbspy: No StackTraces reported");

            return Ok(());
        } else {
            self.0
                .iter()
                .map(|(frame, count)| format!("{} {}", frame, count))
                .collect::<Vec<String>>()
                .iter()
                .try_for_each(|line| writeln!(writer, "{}", line))?;
        }
        Ok(())
    }
}

/// Custom Formatter for Rbspy StackFrames
pub struct StackFrameFormatter<'a>(&'a StackFrame);

impl<'a> std::fmt::Display for StackFrameFormatter<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{} - {}",
            self.0.relative_path, self.0.lineno, self.0.name
        )
    }
}

#[cfg(test)]
mod test_stack_frame_formatter {
    use super::{StackFrame, StackFrameFormatter};

    #[test]
    fn test_stack_frame_formatter() {
        let frame = StackFrame {
            absolute_path: Some("".to_string()),
            relative_path: "test.rb".to_string(),
            lineno: 1,
            name: "test".to_string(),
        };
        let formatter = StackFrameFormatter(&frame);
        assert_eq!(formatter.to_string(), "test.rb:1 - test");
    }
}

/// Rubyspy Writer
/// This object is used to write the output of the rbspy sampler to a data buffer
struct RbspyWriter<'a> {
    data: Vec<u8>,
    buffer: &'a mut Vec<u8>,
}

impl<'a> RbspyWriter<'a> {
    /// Create a new RbspyWriter
    pub fn new(buffer: &'a mut Vec<u8>) -> Self {
        RbspyWriter {
            data: Vec::new(),
            buffer,
        }
    }
}

/// Implement Writer for Rbspy
impl<'a> std::io::Write for RbspyWriter<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // push the data to the buffer
        self.data.extend_from_slice(buf);

        // return the number of bytes written
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // flush the buffer
        self.buffer.extend_from_slice(&self.data);

        Ok(())
    }
}

#[cfg(test)]
mod test_rbspy_writer {
    use super::RbspyWriter;
    use std::io::Write;

    #[test]
    fn test_rbspy_writer() {
        let mut buffer: Vec<u8> = Vec::new();
        let mut writer = RbspyWriter::new(&mut buffer);

        writer.write(b"hello").unwrap();
        writer.write(b"world").unwrap();
        writer.flush().unwrap();

        assert_eq!(buffer, b"helloworld".to_vec());
    }
}
