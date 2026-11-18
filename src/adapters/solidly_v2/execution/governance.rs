//! Event-complete configuration writes for the reviewed volatile fee path.
use super::*;

const FACTORY: Address = address!("8e6a9fcd1d638cc760ffba397605fe3355de76d2");
const FEE: Address = address!("7949de8d67e4f6b8509bdbb43ff0503af1247a7a");
const MEV: Address = address!("5de55ebe5b789bd309809c7969334c2c16e64eaf");
const TAPE: Address = address!("34bdd64b19d8631f936c2d07f19aaa6b83515992");
const QUOTER: Address = address!("95f6b665a1a97878353737ecb3d68aa978a5cb9f");

mod abi {
    alloy_sol_types::sol! {
        event SetFeeManager(address indexed value);
        event SetPauser(address indexed value);
        event SetPauseState(bool indexed value);
        event SetPoolAdmin(address indexed value);
        event SetDefaultFee(uint256 value);
        event SetFeeModule(address indexed value, uint32 gasLimit);
        event SetDiscountRegistryManager(address indexed value);
        event SetDiscountRegistry(address indexed value);
        event SetPoolTape(address indexed value, uint32 gasLimit);
        event SetPoolTapeManager(address indexed value);
        event SetMevTaxModule(address indexed value, uint32 gasLimit);
        event SetExactOutFeeQuoter(address indexed value);
        event SetCustomFee(address indexed pool, uint256 indexed fee);
        event MultiplierSet(uint64 value);
        event MinThresholdSet(uint96 value);
        event BaseFeeFactorSet(uint96 value);
    }
}

fn packed(
    view: &dyn StateView,
    address: Address,
    slot: u64,
    bits: usize,
    shift: usize,
    value: U256,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    let slot = U256::from(slot);
    let old = cell(view, address, slot)?;
    let mask = (U256::MAX >> (256 - bits)) << shift;
    Ok(vec![StateUpdate::slot(
        address,
        slot,
        (old & !mask) | ((value << shift) & mask),
    )])
}

fn binding(
    view: &dyn StateView,
    slot: u64,
    value: Address,
    gas: u32,
    approved: Address,
    zero_allowed: bool,
) -> Result<Vec<StateUpdate>, AdapterEventError> {
    if gas == 0 || (value != approved && !(zero_allowed && value.is_zero())) {
        return Err(invalid(
            "configuration selects an unreviewed module or zero gas limit",
        ));
    }
    packed(
        view,
        FACTORY,
        slot,
        192,
        0,
        U256::from_be_slice(value.as_slice()) | (U256::from(gas) << 160_usize),
    )
}

pub(super) fn event_updates(
    log: &Log,
    view: &dyn StateView,
) -> Result<Option<Vec<StateUpdate>>, AdapterEventError> {
    let Some(topic) = log.topics().first() else {
        return Ok(None);
    };
    macro_rules! decode {
        ($ty:ident) => {
            abi::$ty::decode_log_data_validate(&log.data)
                .map_err(|_| invalid("malformed governance event"))?
        };
    }
    if log.address == FEE && *topic == abi::SetCustomFee::SIGNATURE_HASH {
        let event = decode!(SetCustomFee);
        if event.pool != POOL {
            return Ok(None);
        }
        if event.fee > U256::from(300) && event.fee != U256::from(420) {
            return Err(invalid("invalid custom fee"));
        }
        let slot = solidity_address_mapping_key(POOL, 0);
        let old = cell(view, FEE, slot)?;
        return Ok(Some(vec![StateUpdate::slot(
            FEE,
            slot,
            (old & !(U256::from(0xff_ffff))) | event.fee,
        )]));
    }
    if log.address == MEV {
        let update = if *topic == abi::MultiplierSet::SIGNATURE_HASH {
            packed(
                view,
                MEV,
                1,
                64,
                160,
                U256::from(decode!(MultiplierSet).value),
            )?
        } else if *topic == abi::MinThresholdSet::SIGNATURE_HASH {
            packed(
                view,
                MEV,
                2,
                96,
                0,
                U256::from(decode!(MinThresholdSet).value),
            )?
        } else if *topic == abi::BaseFeeFactorSet::SIGNATURE_HASH {
            packed(
                view,
                MEV,
                2,
                96,
                96,
                U256::from(decode!(BaseFeeFactorSet).value),
            )?
        } else {
            return Ok(None);
        };
        return Ok(Some(update));
    }
    if log.address != FACTORY {
        return Ok(None);
    }
    if *topic == abi::SetDefaultFee::SIGNATURE_HASH {
        let value = decode!(SetDefaultFee).value;
        if value.is_zero() || value > U256::from(300) {
            return Err(invalid("invalid default fee"));
        }
        cell(view, FACTORY, U256::from(5))?;
        return Ok(Some(vec![StateUpdate::slot(FACTORY, U256::from(5), value)]));
    }
    if *topic == abi::SetPauseState::SIGNATURE_HASH {
        return packed(
            view,
            FACTORY,
            4,
            8,
            0,
            U256::from(u8::from(decode!(SetPauseState).value)),
        )
        .map(Some);
    }
    macro_rules! module {
        ($ty:ident, $slot:expr, $approved:expr, $zero:expr) => {
            if *topic == abi::$ty::SIGNATURE_HASH {
                let event = decode!($ty);
                return binding(view, $slot, event.value, event.gasLimit, $approved, $zero)
                    .map(Some);
            }
        };
    }
    module!(SetFeeModule, 7, FEE, false);
    module!(SetPoolTape, 11, TAPE, true);
    module!(SetMevTaxModule, 13, MEV, true);
    let (slot, shift, value) = if *topic == abi::SetPauser::SIGNATURE_HASH {
        (4, 8, decode!(SetPauser).value)
    } else if *topic == abi::SetFeeManager::SIGNATURE_HASH {
        (6, 0, decode!(SetFeeManager).value)
    } else if *topic == abi::SetPoolAdmin::SIGNATURE_HASH {
        (8, 0, decode!(SetPoolAdmin).value)
    } else if *topic == abi::SetDiscountRegistryManager::SIGNATURE_HASH {
        (9, 0, decode!(SetDiscountRegistryManager).value)
    } else if *topic == abi::SetPoolTapeManager::SIGNATURE_HASH {
        (12, 0, decode!(SetPoolTapeManager).value)
    } else if *topic == abi::SetExactOutFeeQuoter::SIGNATURE_HASH {
        let value = decode!(SetExactOutFeeQuoter).value;
        if value != QUOTER {
            return Err(invalid("unreviewed exact-output quoter"));
        }
        (14, 0, value)
    } else if *topic == abi::SetDiscountRegistry::SIGNATURE_HASH {
        let value = decode!(SetDiscountRegistry).value;
        let old = cell(view, FACTORY, U256::from(10))?;
        if value != decode_address_slot(old) {
            return Err(invalid(
                "discount registry replacement requires fresh runtime attestation",
            ));
        }
        (10, 0, value)
    } else {
        return Ok(None);
    };
    if value.is_zero() {
        return Err(invalid("zero governance authority"));
    }
    packed(
        view,
        FACTORY,
        slot,
        160,
        shift,
        U256::from_be_slice(value.as_slice()),
    )
    .map(Some)
}

#[cfg(test)]
mod tests;
