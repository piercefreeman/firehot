use serde::{Deserialize, Serialize};

/// Represents the different types of messages that can be sent between parent and child processes
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MessageType {
    ForkRequest,
    ForkResponse,
    ChildComplete,
    ChildError,
    UnknownCommand,
    UnknownError,
    ImportError,
    ImportComplete,
    ExitRequest,
}

/// Base trait for all messages
pub trait MessageBase {
    fn name(&self) -> MessageType;
}

/// Request to fork a process and execute code
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkRequest {
    pub name: MessageType,
    pub code: String,
}

impl MessageBase for ForkRequest {
    fn name(&self) -> MessageType {
        MessageType::ForkRequest
    }
}

impl ForkRequest {
    pub fn new(code: String) -> Self {
        Self {
            name: MessageType::ForkRequest,
            code,
        }
    }
}

/// Request to exit the process
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitRequest {
    pub name: MessageType,
}

impl MessageBase for ExitRequest {
    fn name(&self) -> MessageType {
        MessageType::ExitRequest
    }
}

impl ExitRequest {
    pub fn new() -> Self {
        Self {
            name: MessageType::ExitRequest,
        }
    }
}

/// Response to a fork request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkResponse {
    pub name: MessageType,
    pub child_pid: i32,
}

impl MessageBase for ForkResponse {
    fn name(&self) -> MessageType {
        MessageType::ForkResponse
    }
}

impl ForkResponse {
    pub fn new(child_pid: i32) -> Self {
        Self {
            name: MessageType::ForkResponse,
            child_pid,
        }
    }
}

/// Message indicating a child process has completed successfully
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildComplete {
    pub name: MessageType,
    pub result: Option<String>,
}

impl MessageBase for ChildComplete {
    fn name(&self) -> MessageType {
        MessageType::ChildComplete
    }
}

impl ChildComplete {
    pub fn new(result: Option<String>) -> Self {
        Self {
            name: MessageType::ChildComplete,
            result,
        }
    }
}

/// Message indicating a child process has encountered an error
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildError {
    pub name: MessageType,
    pub error: String,
}

impl MessageBase for ChildError {
    fn name(&self) -> MessageType {
        MessageType::ChildError
    }
}

impl ChildError {
    pub fn new(error: String) -> Self {
        Self {
            name: MessageType::ChildError,
            error,
        }
    }
}

/// Message indicating an unknown command was received
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnknownCommandError {
    pub name: MessageType,
    pub command: String,
}

impl MessageBase for UnknownCommandError {
    fn name(&self) -> MessageType {
        MessageType::UnknownCommand
    }
}

impl UnknownCommandError {
    pub fn new(command: String) -> Self {
        Self {
            name: MessageType::UnknownCommand,
            command,
        }
    }
}

/// Message indicating an unknown error occurred
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnknownError {
    pub name: MessageType,
    pub error: String,
    pub traceback: Option<String>,
}

impl MessageBase for UnknownError {
    fn name(&self) -> MessageType {
        MessageType::UnknownError
    }
}

impl UnknownError {
    pub fn new(error: String, traceback: Option<String>) -> Self {
        Self {
            name: MessageType::UnknownError,
            error,
            traceback,
        }
    }
}

/// Message indicating an import error occurred
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportError {
    pub name: MessageType,
    pub error: String,
    pub traceback: Option<String>,
}

impl MessageBase for ImportError {
    fn name(&self) -> MessageType {
        MessageType::ImportError
    }
}

impl ImportError {
    pub fn new(error: String, traceback: Option<String>) -> Self {
        Self {
            name: MessageType::ImportError,
            error,
            traceback,
        }
    }
}

/// Message indicating an import was completed successfully
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportComplete {
    pub name: MessageType,
}

impl MessageBase for ImportComplete {
    fn name(&self) -> MessageType {
        MessageType::ImportComplete
    }
}

impl ImportComplete {
    pub fn new() -> Self {
        Self {
            name: MessageType::ImportComplete,
        }
    }
}

/// Enum that can hold any message type
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "name")]
pub enum Message {
    #[serde(rename = "FORK_REQUEST")]
    ForkRequest(ForkRequest),
    #[serde(rename = "FORK_RESPONSE")]
    ForkResponse(ForkResponse),
    #[serde(rename = "CHILD_COMPLETE")]
    ChildComplete(ChildComplete),
    #[serde(rename = "CHILD_ERROR")]
    ChildError(ChildError),
    #[serde(rename = "UNKNOWN_COMMAND")]
    UnknownCommand(UnknownCommandError),
    #[serde(rename = "UNKNOWN_ERROR")]
    UnknownError(UnknownError),
    #[serde(rename = "IMPORT_ERROR")]
    ImportError(ImportError),
    #[serde(rename = "IMPORT_COMPLETE")]
    ImportComplete(ImportComplete),
    #[serde(rename = "EXIT_REQUEST")]
    ExitRequest(ExitRequest),
}

impl Message {
    pub fn name(&self) -> MessageType {
        match self {
            Message::ForkRequest(_) => MessageType::ForkRequest,
            Message::ForkResponse(_) => MessageType::ForkResponse,
            Message::ChildComplete(_) => MessageType::ChildComplete,
            Message::ChildError(_) => MessageType::ChildError,
            Message::UnknownCommand(_) => MessageType::UnknownCommand,
            Message::UnknownError(_) => MessageType::UnknownError,
            Message::ImportError(_) => MessageType::ImportError,
            Message::ImportComplete(_) => MessageType::ImportComplete,
            Message::ExitRequest(_) => MessageType::ExitRequest,
        }
    }
}

/// Helper functions for serialization and deserialization of messages
pub mod io {
    use super::*;
    use serde_json;
    use std::io::{Read, Write};

    /// Write a message to the given writer
    pub fn write_message<W: Write, M: Serialize>(writer: &mut W, message: &M) -> std::io::Result<()> {
        let json = serde_json::to_string(message)?;
        writeln!(writer, "{}", json)?;
        Ok(())
    }

    /// Read a message from the given reader
    pub fn read_message<R: Read>(reader: &mut R) -> Result<Option<Message>, Box<dyn std::error::Error>> {
        let mut buffer = String::new();
        let bytes_read = reader.read_to_string(&mut buffer)?;
        
        if bytes_read == 0 {
            return Ok(None);
        }
        
        let message: Message = serde_json::from_str(&buffer)?;
        Ok(Some(message))
    }
} 