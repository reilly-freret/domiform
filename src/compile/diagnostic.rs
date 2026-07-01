//! Compiler diagnostics.
//!
//! The compiler collects *every* problem it can in one pass rather than bailing
//! on the first — that is most of what makes config feel like a compiled program
//! instead of a thing that explodes at runtime on line 200.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// A single compiler finding. `code` is stable (`E_UNKNOWN_ADAPTER`) so errors
/// can be documented and searched; `context` names the offending entity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub context: Option<String>,
}

impl Diagnostic {
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Error,
            code,
            message: message.into(),
            context: None,
        }
    }

    pub fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            code,
            message: message.into(),
            context: None,
        }
    }

    pub fn at(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(f, "{label}[{}]", self.code)?;
        if let Some(ctx) = &self.context {
            write!(f, " ({ctx})")?;
        }
        write!(f, ": {}", self.message)
    }
}

/// The compiler's error type: a non-empty bag of diagnostics, at least one of
/// which is an `Error` (warnings alone do not fail compilation).
#[derive(Clone, Debug)]
pub struct CompileErrors(pub Vec<Diagnostic>);

impl CompileErrors {
    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.0.iter().filter(|d| d.is_error())
    }
}

impl fmt::Display for CompileErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, d) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{d}")?;
        }
        Ok(())
    }
}

impl std::error::Error for CompileErrors {}
