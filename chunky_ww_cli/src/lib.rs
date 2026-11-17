//! The chunky update commands on a device that serves `ww_dfu` with chunky's `chunky_ww`: `flash` (delta, trial,
//! confirm) with `status`, `confirm`, `revert` and `reboot`. They register as `ww chunky` with `ww`'s command
//! registry ([`wire_weaver_cli::registry`]), so a `ww` binary that links this crate has them with no plugin;
//! the `ww-chunky` binary runs them too.
#![forbid(unsafe_code)]

mod commands;

use anyhow::Result;
use clap::{FromArgMatches, Subcommand};
use wire_weaver_cli::registry::{BoxFuture, CommandDef, Connection, DeviceUse, GlobalFlags, Ran, Run};

pub use commands::{Command, FlashArgs, Into, run_command};

/// `ww chunky`: the commands under one name.
fn command() -> clap::Command {
    Command::augment_subcommands(clap::Command::new("chunky"))
        .about("Updates of chunky devices (delta, trial, confirm): flash, status, confirm, revert, reboot")
        .subcommand_required(true)
        .arg_required_else_help(true)
}

/// Every command talks to the device's `ww_dfu` traits, whatever its API and version.
fn connection(_: &clap::ArgMatches) -> Result<Connection> {
    Ok(Connection::Device(DeviceUse::TRAITS_ONLY))
}

fn run(run: Run<'_>) -> BoxFuture<'_, Result<Ran>> {
    Box::pin(async move {
        let cmd = Command::from_arg_matches(&run.matches)?;
        let device = run.device.take().expect("chunky's connection asked for a device");
        run_command(cmd, device, run.selection(), run.meta).await?;
        // the command owned the connection and dropped it; a reboot or a flash reset the device anyway
        Ok(Ran::DEVICE_GONE)
    })
}

inventory::submit! {
    CommandDef::new("chunky", 125, command, run)
        .globals(&[("", GlobalFlags::Device)])
        .connection(connection)
}

#[cfg(test)]
mod tests {
    use clap::FromArgMatches;

    /// `ww chunky ARGS..` as `ww` parses it: the matches of `chunky`.
    fn parse(args: &[&str]) -> clap::ArgMatches {
        let matches = wire_weaver_cli::cli::root_command()
            .try_get_matches_from(["ww", "chunky"].iter().chain(args))
            .expect("ww parses the command line");
        let (name, sub) = matches.subcommand().expect("a subcommand");
        assert_eq!(name, "chunky");
        sub.clone()
    }

    /// The registry has `chunky`, with every command of `ww-chunky`.
    #[test]
    fn registers_every_chunky_command() {
        let def = wire_weaver_cli::registry::find("chunky").expect("chunky is registered");
        let names: Vec<String> = (def.command)()
            .get_subcommands()
            .map(|c| c.get_name().to_owned())
            .collect();
        assert_eq!(names, ["reboot", "flash", "status", "confirm", "revert"]);
    }

    /// Every command connects to the device, through its traits only.
    #[test]
    fn every_command_connects_traits_only() {
        let def = wire_weaver_cli::registry::find("chunky").expect("chunky is registered");
        for args in [
            &["status"][..],
            &["flash", "fw.elf"],
            &["reboot", "--into", "app"],
            &["confirm"],
            &["revert"],
        ] {
            let connection = (def.connection)(&parse(args)).expect("a connection");
            assert_eq!(
                connection,
                super::Connection::Device(super::DeviceUse::TRAITS_ONLY),
                "{args:?}"
            );
        }
    }

    /// `ww chunky flash` takes the flags of `ww-chunky flash`, with ww's device selection before it.
    #[test]
    fn flash_flags_parse_through_ww() {
        let matches = wire_weaver_cli::cli::root_command()
            .try_get_matches_from([
                "ww",
                "--serial",
                "0B135A01",
                "chunky",
                "flash",
                "fw.elf",
                "--fw-version",
                "1.2.3",
                "--unsigned",
                "--in-flight",
                "4",
                "--no-reboot",
            ])
            .expect("ww parses the command line");
        let (_, sub) = matches.subcommand().expect("a subcommand");
        assert!(matches!(
            super::Command::from_arg_matches(sub),
            Ok(super::Command::Flash(_))
        ));
        let (_, flash) = sub.subcommand().expect("flash");
        assert_eq!(flash.get_one::<u32>("fw_version"), Some(&0x01_0203));
        assert!(flash.get_flag("unsigned"));
        assert_eq!(flash.get_one::<u8>("in_flight"), Some(&4));
    }

    /// A bad version is refused by the parser, before anything connects.
    #[test]
    fn a_bad_version_is_refused() {
        let err = wire_weaver_cli::cli::root_command()
            .try_get_matches_from(["ww", "chunky", "flash", "fw.elf", "--fw-version", "1.2.300"])
            .expect_err("patch above 255");
        assert!(err.to_string().contains("minor and patch up to 255"), "{err}");
    }
}
