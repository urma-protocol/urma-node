use crate::install::Error;
use std::process::{Command, Stdio};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait Runner {
    fn run(&mut self, program: &str, args: &[&str]) -> Result<Outcome, Error>;

    fn require(&mut self, program: &str, args: &[&str]) -> Result<Outcome, Error> {
        let outcome = self.run(program, args)?;
        match outcome.success {
            true => Ok(outcome),
            false => Err(failure(program, args, outcome.stderr.trim().to_owned())),
        }
    }
}

pub struct Commands;

fn failure(program: &str, args: &[&str], detail: String) -> Error {
    Error::Command {
        program: program.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        detail,
    }
}

fn text(program: &str, args: &[&str], bytes: Vec<u8>) -> Result<String, Error> {
    String::from_utf8(bytes).map_err(|error| failure(program, args, error.to_string()))
}

impl Runner for Commands {
    fn run(&mut self, program: &str, args: &[&str]) -> Result<Outcome, Error> {
        let output = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| failure(program, args, error.to_string()))?;
        Ok(Outcome {
            success: output.status.success(),
            stdout: text(program, args, output.stdout)?,
            stderr: text(program, args, output.stderr)?,
        })
    }
}
