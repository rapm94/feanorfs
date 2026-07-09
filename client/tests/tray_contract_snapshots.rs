//! Tray JSON contract snapshots — fail when serialized tray API shapes change.

use feanorfs_common::tray_contract::fixtures;

macro_rules! contract_snapshot {
    ($name:ident, $json:expr) => {
        #[test]
        fn $name() {
            const EXPECTED: &str = include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/snapshots/",
                stringify!($name),
                ".json"
            ));
            assert_eq!($json, EXPECTED.trim());
        }
    };
}

contract_snapshot!(tray_status_json, fixtures::tray_status_json());
contract_snapshot!(recent_workspaces_json, fixtures::recent_workspaces_json());
contract_snapshot!(tray_pause_json, fixtures::tray_pause_json());
contract_snapshot!(conflict_keep_json, fixtures::conflict_keep_json());
contract_snapshot!(conflict_show_json, fixtures::conflict_show_json());
