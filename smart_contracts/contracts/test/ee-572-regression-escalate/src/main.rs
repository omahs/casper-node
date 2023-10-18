#![no_std]
#![no_main]

use casper_contract::contract_api::{runtime, storage};
use casper_types::{AccessRights, AddressableEntityHash, RuntimeArgs, URef};

const REPLACEMENT_DATA: &str = "bawitdaba";
const ARG_CONTRACT_HASH: &str = "contract_hash";

#[no_mangle]
pub extern "C" fn call() {
    let contract_hash: AddressableEntityHash = runtime::get_named_arg(ARG_CONTRACT_HASH);

    let reference: URef = runtime::call_contract(contract_hash, "create", RuntimeArgs::default());
    let forged_reference: URef = URef::new(reference.addr(), AccessRights::READ_ADD_WRITE);
    storage::write(forged_reference, REPLACEMENT_DATA)
}
