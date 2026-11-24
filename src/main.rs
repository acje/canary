//! The canary process: dispatch the invocation, serve one bounded reactor turn
//! at a time, and exit through the closed [`ExitCode`] contract or the health
//! probe's own status.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::time::Instant;

use mio::net::TcpListener;

use canary::config::RuntimePlan;
use canary::exit::ExitCode;
use canary::http::ResponsePlan;
use canary::log::sink::{self, DrainBudget, FLUSH_DRAIN, NonblockingStdout};
use canary::log::{LogStore, Record, WriteOutcome};
use canary::probe::{self, Invocation, ProbeOutcome};
use canary::serve::{Reactor, ShutdownReport, ShutdownSignals, TurnOutcome, TurnWait};

/// The one-line diagnostic an invocation the process does not implement earns.
const USAGE: &str = "usage: canary [--health-check]";

/// Dispatches the invocation before any boot work and exits with its status.
///
/// [`Invocation::HealthCheck`] runs only the probe: no signal handler, log
/// store, or listener exists, and the probe's zero/one contract supplies the
/// status. [`Invocation::Reject`] is a configuration error. Serving is the only
/// path that boots the runtime.
fn main() {
    std::process::exit(process_status(Invocation::parse(
        std::env::args_os().skip(1),
    )));
}

/// The process status `invocation` asks for.
fn process_status(invocation: Invocation) -> i32 {
    match invocation {
        Invocation::Serve => run().code(),
        Invocation::HealthCheck => {
            let outcome = probe::check();
            if let ProbeOutcome::Unhealthy(failure) = outcome {
                eprintln!("canary: health check: {failure}");
            }
            outcome.code()
        }
        Invocation::Reject => failed(USAGE, ExitCode::Config).code(),
    }
}

/// Assembles the runtime plan, serves turns until shutdown, and reports the
/// outcome as an exit code.
///
/// The process exits 0 after a requested shutdown, whatever the final flush
/// managed; 78 when the runtime configuration, the bind, or standard output is
/// rejected at startup; and 70 when a signal, queue, or reactor step fails
/// internally. [`ExitCode`] owns the numeric values.
fn run() -> ExitCode {
    let plan = match RuntimePlan::from_env() {
        Ok(plan) => plan,
        Err(error) => return failed(error, ExitCode::Config),
    };

    let signals = match ShutdownSignals::install() {
        Ok(signals) => signals,
        Err(error) => {
            return failed(
                format_args!("cannot install the shutdown signals: {error}"),
                ExitCode::Software,
            );
        }
    };

    let mut store = match LogStore::new() {
        Ok(store) => store,
        Err(error) => {
            return failed(
                format_args!("cannot reserve the log queue: {error}"),
                ExitCode::Software,
            );
        }
    };

    let mut sink = match NonblockingStdout::open() {
        Ok(sink) => sink,
        Err(error) => {
            return failed(
                format_args!("cannot prepare standard output: {error}"),
                ExitCode::Config,
            );
        }
    };

    let address = SocketAddr::from(([0, 0, 0, 0], plan.listen.port()));
    let listener = match bind_listener(address) {
        Ok(listener) => listener,
        Err(error) => {
            return failed(
                format_args!("cannot bind {address}: {error}"),
                ExitCode::Config,
            );
        }
    };
    let bind = match listener.local_addr() {
        Ok(bind) => bind,
        Err(error) => {
            return failed(
                format_args!("cannot read the bound address: {error}"),
                ExitCode::Software,
            );
        }
    };

    let _ = store.write(plan.filter, Record::Startup { bind });
    let _ = sink::drain_until(
        &mut store,
        &mut sink,
        DrainBudget::DEFAULT,
        Instant::now() + FLUSH_DRAIN,
    );
    if let Some(notice) = plan.notice {
        let _ = store.write(plan.filter, notice);
        let _ = sink::drain(&mut store, &mut sink, DrainBudget::DEFAULT);
    }

    let mut reactor = match Reactor::new(
        listener,
        signals,
        ResponsePlan::new(env!("CARGO_PKG_VERSION")),
    ) {
        Ok(reactor) => reactor,
        Err(error) => return failed(error, ExitCode::Software),
    };

    loop {
        let before = Instant::now();
        if let Err(error) = reactor.wait(before, TurnWait::UntilDeadline) {
            return failed(error, ExitCode::Software);
        }
        match reactor.turn(&mut store, plan.filter, &mut sink, Instant::now()) {
            Ok(TurnOutcome::ShuttingDown) => break,
            Ok(TurnOutcome::Idle | TurnOutcome::Progress) => {}
            Err(error) => return failed(error, ExitCode::Software),
        }
    }

    let report = reactor.shutdown(&mut store, plan.filter, &mut sink, Instant::now());
    if shutdown_flush_incomplete(&report, &store) {
        eprintln!(
            "canary: shutdown flush incomplete: record={:?} drain={:?} leftover={}",
            report.record,
            report.drain,
            store.len()
        );
    }
    ExitCode::Ok
}

/// Whether the final flush lost a record rather than merely filtered one.
///
/// The flush is incomplete when the final [`Record::Shutdown`] was
/// [`WriteOutcome::Dropped`], or when the bounded drain left records queued. A
/// [`WriteOutcome::Written`] or [`WriteOutcome::Filtered`] record over an empty
/// queue lost nothing: the filter is the operator's own choice, and an empty
/// queue means every queued record reached the sink.
fn shutdown_flush_incomplete(report: &ShutdownReport, store: &LogStore) -> bool {
    matches!(report.record, WriteOutcome::Dropped) || !store.is_empty()
}

/// Binds `address` as a nonblocking listener the reactor can own.
///
/// # Errors
///
/// Returns the operating system error when the address cannot be bound or
/// switched to nonblocking mode.
fn bind_listener(address: SocketAddr) -> io::Result<TcpListener> {
    let listener = std::net::TcpListener::bind(address)?;
    listener.set_nonblocking(true)?;
    Ok(TcpListener::from_std(listener))
}

/// Reports `message` to standard error and returns `code`.
fn failed(message: impl fmt::Display, code: ExitCode) -> ExitCode {
    eprintln!("canary: {message}");
    code
}

#[cfg(test)]
mod tests {
    use super::shutdown_flush_incomplete;
    use canary::log::sink::DrainOutcome;
    use canary::log::{LogFilter, LogStore, Record, WriteOutcome};
    use canary::serve::ShutdownReport;

    fn report(record: WriteOutcome) -> ShutdownReport {
        ShutdownReport {
            record,
            drain: DrainOutcome::Idle,
        }
    }

    fn store_with_one_record() -> LogStore {
        let mut store = LogStore::new().unwrap();
        let record = Record::Tick {
            served: 0,
            rejected: 0,
            dropped: 0,
        };
        assert_eq!(store.write(LogFilter::Info, record), WriteOutcome::Written);
        store
    }

    #[test]
    fn a_written_record_over_an_empty_queue_is_a_complete_flush() {
        assert!(!shutdown_flush_incomplete(
            &report(WriteOutcome::Written),
            &LogStore::new().unwrap()
        ));
    }

    #[test]
    fn a_filtered_record_over_an_empty_queue_is_a_complete_flush() {
        assert!(!shutdown_flush_incomplete(
            &report(WriteOutcome::Filtered),
            &LogStore::new().unwrap()
        ));
    }

    #[test]
    fn a_dropped_record_is_an_incomplete_flush() {
        assert!(shutdown_flush_incomplete(
            &report(WriteOutcome::Dropped),
            &LogStore::new().unwrap()
        ));
    }

    #[test]
    fn a_record_left_queued_is_an_incomplete_flush() {
        for record in [WriteOutcome::Written, WriteOutcome::Filtered] {
            assert!(
                shutdown_flush_incomplete(&report(record), &store_with_one_record()),
                "record {record:?}"
            );
        }
    }
}
