//! SDK-1: JSON contract snapshots — fail when serialized agent API shapes change.

feanorfs_test_support::isolate_test_process!();

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
contract_snapshot!(agent_send_json, fixtures::agent_send_json());
contract_snapshot!(agent_message_json, fixtures::agent_message_json());
contract_snapshot!(agent_inbox_json, fixtures::agent_inbox_json());

use feanorfs_common::agent_contract::integrator_fixtures;

contract_snapshot!(
    integrator_assign_json,
    integrator_fixtures::integrator_assign_json()
);
contract_snapshot!(
    integrator_digest_json,
    integrator_fixtures::integrator_digest_json()
);
contract_snapshot!(
    integrator_status_json,
    integrator_fixtures::integrator_status_json()
);
