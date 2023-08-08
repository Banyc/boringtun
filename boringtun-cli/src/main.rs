// Copyright (c) 2019 Cloudflare, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

use boringtun::device::drop_privileges::drop_privileges;
use boringtun::device::{DeviceConfig, DeviceHandle};
use clap::{command, Parser};
use daemonize::Daemonize;
use std::borrow::Cow;
use std::fs::File;
use std::os::unix::net::UnixDatagram;
use std::process::exit;
use tracing::Level;

fn check_tun_name(v: &str) -> Result<String, String> {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        if boringtun::device::tun::parse_utun_name(v).is_ok() {
            Ok(v.to_owned())
        } else {
            Err("Tunnel name must have the format 'utun[0-9]+', use 'utun' for automatic assignment".to_owned())
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(v.to_owned())
    }
}

#[derive(Debug, Parser)]
#[command(author = "Vlad Krasnov <vlad@cloudflare.com>", version = env!("CARGO_PKG_VERSION"))]
struct Args {
    /// The name of the created interface
    #[clap(value_parser = check_tun_name)]
    interface_name: String,

    /// Run and log in the foreground
    #[clap(long, short)]
    foreground: bool,

    /// Number of OS threads to use
    #[clap(long, short, env = "WG_THREADS", default_value_t = 4)]
    threads: usize,

    /// Log verbosity
    #[clap(long, short, env = "WG_LOG_LEVEL", default_value_t = Level::ERROR)]
    verbosity: Level,

    /// File descriptor for the user API. Linux only.
    #[clap(long, env = "WG_UAPI_FD", default_value_t = -1)]
    uapi_fd: i32,

    /// File descriptor for an already-existing TUN device
    #[clap(long, env = "WG_TUN_FD", default_value_t = -1)]
    tun_fd: i32,

    /// Log file
    #[clap(long, short, env = "WG_LOG_FILE", default_value_t = String::from("/tmp/boringtun.out"))]
    log: String,

    /// Do not drop sudo privileges
    #[clap(long, env = "WG_SUDO")]
    disable_drop_privileges: bool,

    /// Disable connected UDP sockets to each peer
    #[clap(long)]
    disable_connected_udp: bool,

    /// Disable using multiple queues for the tunnel interface. Linux only.
    #[clap(long)]
    disable_multi_queue: bool,
}

impl Args {
    pub fn tun_name(&self) -> Cow<'_, str> {
        if self.tun_fd >= 0 {
            return Cow::from(self.tun_fd.to_string());
        }
        Cow::from(&self.interface_name)
    }
}

fn main() {
    let args = Args::parse();

    // Create a socketpair to communicate between forked processes
    let (sock1, sock2) = UnixDatagram::pair().unwrap();
    let _ = sock1.set_nonblocking(true);

    let _guard;

    if !args.foreground {
        let log_file = File::create(&args.log)
            .unwrap_or_else(|_| panic!("Could not create log file {}", args.log));

        let (non_blocking, guard) = tracing_appender::non_blocking(log_file);

        _guard = guard;

        tracing_subscriber::fmt()
            .with_max_level(args.verbosity)
            .with_writer(non_blocking)
            .with_ansi(false)
            .init();

        let daemonize = Daemonize::new()
            .working_directory("/tmp")
            .exit_action(move || {
                let mut b = [0u8; 1];
                if sock2.recv(&mut b).is_ok() && b[0] == 1 {
                    println!("BoringTun started successfully");
                } else {
                    eprintln!("BoringTun failed to start");
                    exit(1);
                };
            });

        match daemonize.start() {
            Ok(_) => tracing::info!("BoringTun started successfully"),
            Err(e) => {
                tracing::error!(error = ?e);
                exit(1);
            }
        }
    } else {
        tracing_subscriber::fmt()
            .pretty()
            .with_max_level(args.verbosity)
            .init();
    }

    let config = DeviceConfig {
        n_threads: args.threads,
        #[cfg(target_os = "linux")]
        uapi_fd: args.uapi_fd,
        use_connected_socket: !args.disable_connected_udp,
        #[cfg(target_os = "linux")]
        use_multi_queue: !args.disable_multi_queue,
    };

    let mut device_handle: DeviceHandle = match DeviceHandle::new(&args.tun_name(), config) {
        Ok(d) => d,
        Err(e) => {
            // Notify parent that tunnel initialization failed
            tracing::error!(message = "Failed to initialize tunnel", error = ?e);
            sock1.send(&[0]).unwrap();
            exit(1);
        }
    };

    if !args.disable_drop_privileges {
        if let Err(e) = drop_privileges() {
            tracing::error!(message = "Failed to drop privileges", error = ?e);
            sock1.send(&[0]).unwrap();
            exit(1);
        }
    }

    // Notify parent that tunnel initialization succeeded
    sock1.send(&[1]).unwrap();
    drop(sock1);

    tracing::info!("BoringTun started successfully");

    device_handle.wait();
}
