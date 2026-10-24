// SPDX-License-Identifier: GPL-3.0-or-later
//! From probed commands (plus available CLIs) to the capability set reported to clients.

use kicad_cli_runner::KicadInstall;
use kicad_domain::{Backend, Capability, CapabilitySet, Support};
use kicad_ipc::capabilities::{command, CommandSupport, ProbeReport};

fn ipc(report: &ProbeReport, name: &str) -> Support {
    match report.commands.get(name) {
        Some(CommandSupport::Supported) => Support::via(Backend::KicadIpc),
        Some(CommandSupport::Unhandled) => Support::unavailable(format!("this KiCad does not handle {name}")),
        Some(CommandSupport::Unknown(why)) => Support::unavailable(format!("{name} could not be probed: {why}")),
        None => Support::unavailable(format!("{name} was not probed")),
    }
}

/// `headless` selects behaviour that only a headless server has (document lifecycle commands).
/// `cli` is the kicad-cli install matching the session's KiCad version, if any.
pub fn capabilities_from_probe(report: &ProbeReport, headless: bool, cli: Option<&KicadInstall>) -> CapabilitySet {
    let mut caps = CapabilitySet::default();
    let cli_support = |what: &str| match cli {
        Some(install) => Support::via(Backend::KicadCli).with_note(format!(
            "{what} via kicad-cli {} on the saved file",
            install.version.full
        )),
        None => Support::unavailable(format!(
            "no kicad-cli install matching this KiCad version was found for {what}"
        )),
    };

    caps.set(Capability::SessionStatus, Support::via(Backend::KicadIpc));
    caps.set(
        Capability::SessionOpenDocuments,
        ipc(report, command::GET_OPEN_DOCUMENTS),
    );
    caps.set(Capability::Commits, ipc(report, command::BEGIN_COMMIT));
    caps.set(
        Capability::SchReadHierarchy,
        ipc(report, command::GET_SCHEMATIC_HIERARCHY),
    );
    caps.set(Capability::SchReadSymbols, ipc(report, command::GET_ITEMS));

    let lifecycle = ipc(report, command::GET_DOCUMENT_MODIFIED_STATE);
    caps.set(
        Capability::DocumentLifecycle,
        if headless {
            lifecycle.clone()
        } else {
            Support::unavailable("document lifecycle commands are headless-only")
        },
    );
    // SaveDocument cannot be probed without writing the file; it ships alongside the modified-state
    // query in the same proto, so its support is inferred from that.
    caps.set(
        Capability::SchSave,
        if lifecycle.available {
            lifecycle.with_note("inferred from GetDocumentModifiedState")
        } else {
            lifecycle
        },
    );

    let netlist = ipc(report, command::GET_SCHEMATIC_NETLIST);
    caps.set(
        Capability::SchNetlist,
        if netlist.available {
            netlist
        } else {
            cli_support("netlist")
        },
    );

    // KiCad 10 accepts an empty CreateItems batch without supporting any schematic item type, so
    // editing needs both the placement and the creation command.
    let place = ipc(report, command::PLACE_SYMBOL_FROM_LIBRARY);
    let create = ipc(report, command::CREATE_ITEMS);
    let edit = if place.available && create.available {
        create
    } else if !place.available {
        place.clone()
    } else {
        create
    };
    caps.set(Capability::SchPlaceSymbol, place);
    caps.set(Capability::SchEditItems, edit);

    caps.set(Capability::SchErc, cli_support("ERC"));
    caps.set(Capability::SchRender, cli_support("rendering"));
    caps.set(Capability::SchExportNetlist, cli_support("netlist export"));
    caps
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn report(entries: &[(&'static str, CommandSupport)]) -> ProbeReport {
        ProbeReport {
            kicad_version: "test".into(),
            major: 10,
            minor: 0,
            patch: 7,
            instance_token: None,
            schematics: vec![],
            commands: entries.iter().cloned().collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn kicad10_gui_cannot_edit_even_though_empty_create_items_succeeds() {
        let r = report(&[
            (command::GET_OPEN_DOCUMENTS, CommandSupport::Supported),
            (command::BEGIN_COMMIT, CommandSupport::Supported),
            (command::CREATE_ITEMS, CommandSupport::Supported),
            (command::PLACE_SYMBOL_FROM_LIBRARY, CommandSupport::Unhandled),
            (command::GET_SCHEMATIC_NETLIST, CommandSupport::Unhandled),
        ]);
        let caps = capabilities_from_probe(&r, false, None);
        assert!(!caps.is_available(Capability::SchEditItems));
        assert!(!caps.is_available(Capability::SchPlaceSymbol));
        assert!(caps.is_available(Capability::Commits));
        assert!(!caps.is_available(Capability::SchNetlist), "no CLI either");
        assert!(!caps.is_available(Capability::SchErc));
    }

    #[test]
    fn kicad11_headless() {
        let r = report(&[
            (command::GET_OPEN_DOCUMENTS, CommandSupport::Supported),
            (command::BEGIN_COMMIT, CommandSupport::Unhandled),
            (command::CREATE_ITEMS, CommandSupport::Supported),
            (command::PLACE_SYMBOL_FROM_LIBRARY, CommandSupport::Supported),
            (command::GET_SCHEMATIC_NETLIST, CommandSupport::Supported),
            (command::GET_DOCUMENT_MODIFIED_STATE, CommandSupport::Supported),
        ]);
        let caps = capabilities_from_probe(&r, true, None);
        for c in [
            Capability::SchEditItems,
            Capability::SchPlaceSymbol,
            Capability::SchNetlist,
            Capability::DocumentLifecycle,
            Capability::SchSave,
        ] {
            assert!(caps.is_available(c), "{c:?}");
        }
        assert_eq!(
            caps.get(Capability::SchNetlist).unwrap().backend,
            Some(Backend::KicadIpc)
        );
        assert!(!caps.is_available(Capability::Commits));
    }
}
