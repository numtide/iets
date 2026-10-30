//! What the daemon reports while it builds, decoded from the `STDERR_*` stream.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Started(Started),
    Log(Log),
    Failed(Failed),
    Phase(Phase),
    Stopped(Stopped),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub drv_path: String,
}

/// `drv_path` is empty when the daemon did not attribute the line to a build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Log {
    pub drv_path: String,
    pub line: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed {
    pub drv_path: String,
    pub reason: String,
}

/// A build's `setPhase` result (logging.hh:resSetPhase).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase {
    pub drv_path: String,
    pub phase: String,
}

/// The build activity ended. `built` is true when the backend saw the build
/// succeed. False says nothing, the op's `BuildResult` and the daemon's error
/// messages decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stopped {
    pub drv_path: String,
    pub built: bool,
}
