//! SDK-1: JSON contract snapshots — fail when serialized agent API shapes change.

use feanorfs_common::agent_contract::fixtures;

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

contract_snapshot!(spawn_json, fixtures::spawn_json());
contract_snapshot!(agent_list_json, fixtures::agent_list_json());
contract_snapshot!(agent_list_offline_json, fixtures::agent_list_offline_json());
contract_snapshot!(agent_check_json, fixtures::agent_check_json());
contract_snapshot!(agent_land_json, fixtures::agent_land_json());
contract_snapshot!(agent_refresh_json, fixtures::agent_refresh_json());
contract_snapshot!(agent_clean_json, fixtures::agent_clean_json());
contract_snapshot!(log_json, fixtures::log_json());
contract_snapshot!(undo_json, fixtures::undo_json());
