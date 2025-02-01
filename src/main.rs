use argh::FromArgs;
use my_parachain::runtime_types::sp_weights::weight_v2::Weight;
use my_parachain::runtime_types::{staging_xcm, xcm};
use std::str::FromStr;
use std::sync::LazyLock;
use subxt::config::polkadot::AccountId32;
use subxt::ext::codec::Decode;
use subxt::tx::Payload;
use subxt::Metadata;
use subxt::{OnlineClient, PolkadotConfig};

pub static RELAY_CHAIN_METADATA: LazyLock<Metadata> = LazyLock::new(|| {
    let bytes = std::fs::read("./artifacts/kusama.scale").expect("missing metadata file");
    Metadata::decode(&mut &*bytes).expect("invalid encoding")
});

pub static MY_PARACHAIN_METADATA: LazyLock<Metadata> = LazyLock::new(|| {
    let bytes = std::fs::read("./artifacts/my-parachain.scale").expect("missing metadata file");
    Metadata::decode(&mut &*bytes).expect("invalid encoding")
});

#[subxt::subxt(runtime_metadata_path = "./artifacts/kusama.scale")]
pub mod relay_chain {}

#[subxt::subxt(runtime_metadata_path = "./artifacts/my-parachain.scale")]
pub mod my_parachain {}


/// Reach new heights.
#[derive(FromArgs)]
struct Args {
    /// your parachain id.
    #[argh(option)]
    para_id: u32,

    /// an address on the relay chain to receive the surplus of the funds after XCM execution.
    #[argh(option)]
    refund_account: String,
}

async fn check_locked_status(rc_client: &OnlineClient<PolkadotConfig>, para_id: u32) -> Result<bool, Box<dyn std::error::Error>> {
    let storage_query = relay_chain::storage().registrar().paras(
        &relay_chain::runtime_types::polkadot_parachain_primitives::primitives::Id(para_id),
    );
    let result = rc_client
        .storage()
        .at_latest()
        .await?
        .fetch(&storage_query)
        .await?
        .map(|a| a.locked.unwrap_or(false))
        .unwrap_or(false); // TODO: handle errors
    Ok(result)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Args = argh::from_env();
    let para_id = args.para_id;
    let refund_account = AccountId32::from_str(&args.refund_account)?;

    let api = OnlineClient::<PolkadotConfig>::from_url("wss://kusama-rpc.polkadot.io").await?;

    let location = relay_chain::runtime_types::xcm::VersionedLocation::V4(
        relay_chain::runtime_types::staging_xcm::v4::location::Location {
            parents: 0,
            interior: relay_chain::runtime_types::staging_xcm::v4::junctions::Junctions::X1([
                relay_chain::runtime_types::staging_xcm::v4::junction::Junction::Parachain(para_id),
            ]),
        },
    );

    let lock_status = check_locked_status(&api, para_id).await?;
    if !lock_status {
        println!("Parachain {para_id} is not locked, skipping the XCM unlock");
        return Ok(());
    }

    let runtime_api_call = relay_chain::apis()
        .location_to_account_api()
        .convert_location(location);
    let address = api
        .runtime_api()
        .at_latest()
        .await?
        .call(runtime_api_call)
        .await?
        .expect("should be OK");

    println!("Sovereign account of {para_id}: {address}");

    let account = address.into();
    let storage_query = relay_chain::storage().system().account(&account);
    let balance = api
        .storage()
        .at_latest()
        .await?
        .fetch(&storage_query)
        .await?
        .map(|a| a.data.free)
        .unwrap_or(0);

    println!("Sovereign account balance: {balance} UNITs");

    if balance == 0 {
        println!("Sovereign account balance is zero, skipping the XCM unlock");
        return Ok(());
    }

    let unlock_call = relay_chain::tx().registrar().remove_lock(
        relay_chain::runtime_types::polkadot_parachain_primitives::primitives::Id(para_id),
    );
    let unlock_call_encoded = unlock_call.encode_call_data(&RELAY_CHAIN_METADATA)?;

    let location = xcm::VersionedLocation::V4(staging_xcm::v4::location::Location {
        parents: 1,
        interior: staging_xcm::v4::junctions::Junctions::Here,
    });
    let fees = 1_000_000_000; // TODO
    let asset = staging_xcm::v4::asset::Asset {
        id: staging_xcm::v4::asset::AssetId(staging_xcm::v4::location::Location {
            parents: 0,
            interior: staging_xcm::v4::junctions::Junctions::Here,
        }),
        fun: staging_xcm::v4::asset::Fungibility::Fungible(fees),
    };
    let asset_clone = staging_xcm::v4::asset::Asset {
        id: staging_xcm::v4::asset::AssetId(staging_xcm::v4::location::Location {
            parents: 0,
            interior: staging_xcm::v4::junctions::Junctions::Here,
        }),
        fun: staging_xcm::v4::asset::Fungibility::Fungible(fees),
    };

    let instructions = vec![
        staging_xcm::v4::Instruction::WithdrawAsset(staging_xcm::v4::asset::Assets(vec![
            asset_clone,
        ])),
        staging_xcm::v4::Instruction::BuyExecution {
            fees: asset,
            weight_limit: xcm::v3::WeightLimit::Unlimited,
        },
        staging_xcm::v4::Instruction::Transact {
            origin_kind: xcm::v3::OriginKind::Native,
            require_weight_at_most: Weight {
                ref_time: 1_000_000_000,
                proof_size: 100_000,
            },
            call: xcm::double_encoded::DoubleEncoded {
                encoded: unlock_call_encoded,
            },
        },
        staging_xcm::v4::Instruction::RefundSurplus,
        staging_xcm::v4::Instruction::DepositAsset {
            assets: staging_xcm::v4::asset::AssetFilter::Wild(
                staging_xcm::v4::asset::WildAsset::All,
            ),
            beneficiary: staging_xcm::v4::location::Location {
                parents: 0,
                interior: staging_xcm::v4::junctions::Junctions::X1([
                    staging_xcm::v4::junction::Junction::AccountId32 {
                        network: None,
                        id: refund_account.0,
                    },
                ]),
            },
        },
    ];
    let xcm_message = xcm::VersionedXcm::V4(staging_xcm::v4::Xcm(instructions));

    let xcm_call = my_parachain::Call::PolkadotXcm(
        my_parachain::runtime_types::pallet_xcm::pallet::Call::send {
            dest: Box::new(location),
            message: Box::new(xcm_message),
        },
    );
    let sudo_tx = my_parachain::tx().sudo().sudo_unchecked_weight(
        xcm_call,
        Weight {
            ref_time: 0,
            proof_size: 0,
        },
    );

    let encoded = sudo_tx.encode_call_data(&*MY_PARACHAIN_METADATA)?;

    println!("0x{}", hex::encode(&encoded));
    Ok(())
}
