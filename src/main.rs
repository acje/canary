//! The canary process: bind the liveness listener, serve one bounded reactor
//! turn at a time, and exit through the closed [`ExitCode`] contract.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::time::Instant;

use mio::net::TcpListener;

use canary::config::RuntimePlan;
use canary::exit::ExitCode;
use canary::http::ResponsePlan;
use canary::log::sink::{self, DrainBudget, DrainOutcome, NonblockingStdout, SHUTDOWN_DRAIN};
use canary::log::{LogStore, Record, WriteOutcome};
use canary::serve::{Reactor, ShutdownSignals, TurnOutcome, TurnWait};

/// Serves the liveness probe until a shutdown signal and exits with the
/// [`ExitCode`] of the outcome.
///
/// The process exits 0 after a requested shutdown, whatever the final flush
/// managed; 78 when the runtime configuration, the bind, or standard output is
/// rejected at startup; and 70 when a signal, queue, or reactor step fails
/// internally. [`ExitCode`] owns the numeric values.
fn main() {
    std::process::exit(run().code());
}

/// Assembles the runtime plan, serves turns until shutdown, and reports the
/// outcome as an exit code.
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
        Instant::now() + SHUTDOWN_DRAIN,
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
    if report.record != WriteOutcome::Written || report.drain != DrainOutcome::Idle {
        eprintln!(
            "canary: shutdown flush incomplete: record={:?} drain={:?}",
            report.record, report.drain
        );
    }
    ExitCode::Ok
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
