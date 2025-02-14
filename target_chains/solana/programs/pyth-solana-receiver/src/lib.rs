use {
    crate::error::ReceiverError,
    anchor_lang::prelude::*,
    pythnet_sdk::{
        accumulators::merkle::MerkleRoot,
        hashers::keccak256_160::Keccak160,
        messages::Message,
        wire::{
            from_slice,
            v1::{
                MerklePriceUpdate,
                WormholeMessage,
                WormholePayload,
            },
        },
    },
    solana_program::{
        keccak,
        program_memory::sol_memcpy,
        secp256k1_recover::secp256k1_recover,
        system_instruction,
    },
    state::{
        config::{
            Config,
            DataSource,
        },
        price_update::{
            PriceUpdateV1,
            VerificationLevel,
        },
    },
    wormhole_core_bridge_solana::{
        sdk::{
            legacy::AccountVariant,
            VaaAccount,
        },
        state::GuardianSet,
    },
    wormhole_raw_vaas::{
        GuardianSetSig,
        Vaa,
    },
};

pub mod error;
pub mod sdk;
pub mod state;

declare_id!("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ");

#[program]
pub mod pyth_solana_receiver {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>, initial_config: Config) -> Result<()> {
        let config = &mut ctx.accounts.config;
        **config = initial_config;
        Ok(())
    }

    pub fn request_governance_authority_transfer(
        ctx: Context<Governance>,
        target_governance_authority: Pubkey,
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.target_governance_authority = Some(target_governance_authority);
        Ok(())
    }

    pub fn authorize_governance_authority_transfer(
        ctx: Context<AuthorizeGovernanceAuthorityTransfer>,
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.governance_authority = config.target_governance_authority.ok_or(error!(
            ReceiverError::NonexistentGovernanceAuthorityTransferRequest
        ))?;
        config.target_governance_authority = None;
        Ok(())
    }

    pub fn set_data_sources(
        ctx: Context<Governance>,
        valid_data_sources: Vec<DataSource>,
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.valid_data_sources = valid_data_sources;
        Ok(())
    }

    pub fn set_fee(ctx: Context<Governance>, single_update_fee_in_lamports: u64) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.single_update_fee_in_lamports = single_update_fee_in_lamports;
        Ok(())
    }

    pub fn set_wormhole_address(ctx: Context<Governance>, wormhole: Pubkey) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.wormhole = wormhole;
        Ok(())
    }

    pub fn set_minimum_signatures(ctx: Context<Governance>, minimum_signatures: u8) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.minimum_signatures = minimum_signatures;
        Ok(())
    }

    /// Post a price update using a VAA and a MerklePriceUpdate.
    /// This function allows you to post a price update in a single transaction.
    /// Compared to post_updates, it is less secure since you won't be able to verify all guardian signatures if you use this function because of transaction size limitations.
    /// Typically, you can fit 5 guardian signatures in a transaction that uses this.
    pub fn post_updates_atomic(
        ctx: Context<PostUpdatesAtomic>,
        params: PostUpdatesAtomicParams,
    ) -> Result<()> {
        let config = &ctx.accounts.config;
        let guardian_set =
            deserialize_guardian_set_checked(&ctx.accounts.guardian_set, &config.wormhole)?;

        // This section is borrowed from https://github.com/wormhole-foundation/wormhole/blob/wen/solana-rewrite/solana/programs/core-bridge/src/processor/parse_and_verify_vaa/verify_encoded_vaa_v1.rs#L59
        let vaa = Vaa::parse(&params.vaa).map_err(|_| ReceiverError::DeserializeVaaFailed)?;
        // Must be V1.
        require_eq!(vaa.version(), 1, ReceiverError::InvalidVaaVersion);

        // Make sure the encoded guardian set index agrees with the guardian set account's index.
        let guardian_set = guardian_set.inner();
        require_eq!(
            vaa.guardian_set_index(),
            guardian_set.index,
            ReceiverError::GuardianSetMismatch
        );

        let guardian_keys = &guardian_set.keys;

        // Generate the same message hash (using keccak) that the Guardians used to generate their
        // signatures. This message hash will be hashed again to produce the digest for
        // `secp256k1_recover`.
        let digest = keccak::hash(keccak::hash(vaa.body().as_ref()).as_ref());

        let mut last_guardian_index = None;
        for sig in vaa.signatures() {
            // We do not allow for non-increasing guardian signature indices.
            let index = usize::from(sig.guardian_index());
            if let Some(last_index) = last_guardian_index {
                require!(index > last_index, ReceiverError::InvalidGuardianIndex);
            }

            // Does this guardian index exist in this guardian set?
            let guardian_pubkey = guardian_keys
                .get(index)
                .ok_or_else(|| error!(ReceiverError::InvalidGuardianIndex))?;

            // Now verify that the signature agrees with the expected Guardian's pubkey.
            verify_guardian_signature(&sig, guardian_pubkey, digest.as_ref())?;

            last_guardian_index = Some(index);
        }
        // End borrowed section

        let payer = &ctx.accounts.payer;
        let treasury = &ctx.accounts.treasury;
        let price_update_account = &mut ctx.accounts.price_update_account;

        require_gte!(
            vaa.signature_count(),
            config.minimum_signatures,
            ReceiverError::InsufficientGuardianSignatures
        );

        let vaa_components = VaaComponents {
            verification_level: VerificationLevel::Partial(vaa.signature_count()),
            emitter_address:    vaa.body().emitter_address(),
            emitter_chain:      vaa.body().emitter_chain(),
        };

        post_price_update_from_vaa(
            config,
            payer,
            treasury,
            price_update_account,
            &vaa_components,
            vaa.payload().as_ref(),
            &params.merkle_price_update,
        )?;


        Ok(())
    }

    /// Post a price update using an encoded_vaa account and a MerklePriceUpdate calldata.
    /// This should be called after the client has already verified the Vaa via the Wormhole contract.
    /// Check out target_chains/solana/cli/src/main.rs for an example of how to do this.
    pub fn post_updates(ctx: Context<PostUpdates>, price_update: MerklePriceUpdate) -> Result<()> {
        let config = &ctx.accounts.config;
        let payer: &Signer<'_> = &ctx.accounts.payer;
        let encoded_vaa = VaaAccount::load(&ctx.accounts.encoded_vaa)?;
        let treasury: &AccountInfo<'_> = &ctx.accounts.treasury;
        let price_update_account: &mut Account<'_, PriceUpdateV1> =
            &mut ctx.accounts.price_update_account;

        let vaa_components = VaaComponents {
            verification_level: VerificationLevel::Full,
            emitter_address:    encoded_vaa.try_emitter_address()?,
            emitter_chain:      encoded_vaa.try_emitter_chain()?,
        };

        post_price_update_from_vaa(
            config,
            payer,
            treasury,
            price_update_account,
            &vaa_components,
            encoded_vaa.try_payload()?.as_ref(),
            &price_update,
        )?;

        Ok(())
    }
}

pub const CONFIG_SEED: &str = "config";
pub const TREASURY_SEED: &str = "treasury";

#[derive(Accounts)]
#[instruction(initial_config : Config)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub payer:          Signer<'info>,
    #[account(init, space = Config::LEN, payer=payer, seeds = [CONFIG_SEED.as_ref()], bump)]
    pub config:         Account<'info, Config>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Governance<'info> {
    #[account(constraint =
        payer.key() == config.governance_authority @
        ReceiverError::GovernanceAuthorityMismatch
    )]
    pub payer:  Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED.as_ref()], bump)]
    pub config: Account<'info, Config>,
}

#[derive(Accounts)]
pub struct AuthorizeGovernanceAuthorityTransfer<'info> {
    #[account(constraint =
        payer.key() == config.target_governance_authority.ok_or(error!(ReceiverError::NonexistentGovernanceAuthorityTransferRequest))? @
        ReceiverError::TargetGovernanceAuthorityMismatch
    )]
    pub payer:  Signer<'info>,
    #[account(seeds = [CONFIG_SEED.as_ref()], bump)]
    pub config: Account<'info, Config>,
}

#[derive(Accounts)]
pub struct PostUpdates<'info> {
    #[account(mut)]
    pub payer:                Signer<'info>,
    #[account(owner = config.wormhole)]
    /// CHECK: We aren't deserializing the VAA here but later with VaaAccount::load, which is the recommended way
    pub encoded_vaa:          AccountInfo<'info>,
    #[account(seeds = [CONFIG_SEED.as_ref()], bump)]
    pub config:               Account<'info, Config>,
    #[account(seeds = [TREASURY_SEED.as_ref()], bump)]
    /// CHECK: This is just a PDA controlled by the program. There is currently no way to withdraw funds from it.
    #[account(mut)]
    pub treasury:             AccountInfo<'info>,
    /// The contraint is such that either the price_update_account is uninitialized or the payer is the write_authority.
    /// Pubkey::default() is the SystemProgram on Solana and it can't sign so it's impossible that price_update_account.write_authority == Pubkey::default() once the account is initialized
    #[account(init_if_needed, constraint = price_update_account.write_authority == Pubkey::default() || price_update_account.write_authority == payer.key(), payer =payer, space = PriceUpdateV1::LEN)]
    pub price_update_account: Account<'info, PriceUpdateV1>,
    pub system_program:       Program<'info, System>,
}

#[derive(Accounts)]
pub struct PostUpdatesAtomic<'info> {
    #[account(mut)]
    pub payer:                Signer<'info>,
    /// CHECK: We can't use AccountVariant::<GuardianSet> here because its owner is hardcoded as the "official" Wormhole program and we want to get the wormhole address from the config.
    /// Instead we do the same steps in deserialize_guardian_set_checked.
    #[account(
        owner = config.wormhole)]
    pub guardian_set:         AccountInfo<'info>,
    #[account(seeds = [CONFIG_SEED.as_ref()], bump)]
    pub config:               Account<'info, Config>,
    #[account(mut, seeds = [TREASURY_SEED.as_ref()], bump)]
    /// CHECK: This is just a PDA controlled by the program. There is currently no way to withdraw funds from it.
    pub treasury:             AccountInfo<'info>,
    /// The contraint is such that either the price_update_account is uninitialized or the payer is the write_authority.
    /// Pubkey::default() is the SystemProgram on Solana and it can't sign so it's impossible that price_update_account.write_authority == Pubkey::default() once the account is initialized
    #[account(init_if_needed, constraint = price_update_account.write_authority == Pubkey::default() || price_update_account.write_authority == payer.key(), payer = payer, space = PriceUpdateV1::LEN)]
    pub price_update_account: Account<'info, PriceUpdateV1>,
    pub system_program:       Program<'info, System>,
}

#[derive(Debug, AnchorSerialize, AnchorDeserialize, Clone)]
pub struct PostUpdatesAtomicParams {
    pub vaa:                 Vec<u8>,
    pub merkle_price_update: MerklePriceUpdate,
}


fn deserialize_guardian_set_checked(
    account_info: &AccountInfo<'_>,
    wormhole: &Pubkey,
) -> Result<AccountVariant<GuardianSet>> {
    let mut guardian_set_data: &[u8] = &account_info.try_borrow_data()?;
    let guardian_set =
        AccountVariant::<GuardianSet>::try_deserialize_unchecked(&mut guardian_set_data)?;

    let expected_address = Pubkey::find_program_address(
        &[
            GuardianSet::SEED_PREFIX,
            guardian_set.inner().index.to_be_bytes().as_ref(),
        ],
        wormhole,
    )
    .0;

    require!(
        expected_address == *account_info.key,
        ReceiverError::InvalidGuardianSetPda
    );

    Ok(guardian_set)
}


struct VaaComponents {
    verification_level: VerificationLevel,
    emitter_address:    [u8; 32],
    emitter_chain:      u16,
}

fn post_price_update_from_vaa<'info>(
    config: &Account<'info, Config>,
    payer: &Signer<'info>,
    treasury: &AccountInfo<'info>,
    price_update_account: &mut Account<'_, PriceUpdateV1>,
    vaa_components: &VaaComponents,
    vaa_payload: &[u8],
    price_update: &MerklePriceUpdate,
) -> Result<()> {
    if payer.lamports()
        < Rent::get()?
            .minimum_balance(0)
            .saturating_add(config.single_update_fee_in_lamports)
    {
        return err!(ReceiverError::InsufficientFunds);
    };

    let transfer_instruction = system_instruction::transfer(
        payer.key,
        treasury.key,
        config.single_update_fee_in_lamports,
    );
    anchor_lang::solana_program::program::invoke(
        &transfer_instruction,
        &[payer.to_account_info(), treasury.to_account_info()],
    )?;

    let valid_data_source = config.valid_data_sources.iter().any(|x| {
        *x == DataSource {
            chain:   vaa_components.emitter_chain,
            emitter: Pubkey::from(vaa_components.emitter_address),
        }
    });
    if !valid_data_source {
        return err!(ReceiverError::InvalidDataSource);
    }

    let wormhole_message = WormholeMessage::try_from_bytes(vaa_payload)
        .map_err(|_| ReceiverError::InvalidWormholeMessage)?;
    let root: MerkleRoot<Keccak160> = MerkleRoot::new(match wormhole_message.payload {
        WormholePayload::Merkle(merkle_root) => merkle_root.root,
    });

    if !root.check(price_update.proof.clone(), price_update.message.as_ref()) {
        return err!(ReceiverError::InvalidPriceUpdate);
    }

    let message = from_slice::<byteorder::BE, Message>(price_update.message.as_ref())
        .map_err(|_| ReceiverError::DeserializeMessageFailed)?;

    match message {
        Message::PriceFeedMessage(price_feed_message) => {
            price_update_account.write_authority = payer.key();
            price_update_account.verification_level = vaa_components.verification_level;
            price_update_account.price_message = price_feed_message;
        }
        Message::TwapMessage(_) => {
            return err!(ReceiverError::UnsupportedMessageType);
        }
    }
    Ok(())
}

/**
 * Borrowed from https://github.com/wormhole-foundation/wormhole/blob/wen/solana-rewrite/solana/programs/core-bridge/src/processor/parse_and_verify_vaa/verify_encoded_vaa_v1.rs#L121
 */
fn verify_guardian_signature(
    sig: &GuardianSetSig,
    guardian_pubkey: &[u8; 20],
    digest: &[u8],
) -> Result<()> {
    // Recover using `solana_program::secp256k1_recover`. Public key recovery costs 25k compute
    // units. And hashing this public key to recover the Ethereum public key costs about 13k.
    let recovered = {
        // Recover EC public key (64 bytes).
        let pubkey = secp256k1_recover(digest, sig.recovery_id(), &sig.rs())
            .map_err(|_| ReceiverError::InvalidSignature)?;

        // The Ethereum public key is the last 20 bytes of keccak hashed public key above.
        let hashed = keccak::hash(&pubkey.to_bytes());

        let mut eth_pubkey = [0; 20];
        sol_memcpy(&mut eth_pubkey, &hashed.0[12..], 20);

        eth_pubkey
    };

    // The recovered public key should agree with the Guardian's public key at this index.
    require!(
        recovered == *guardian_pubkey,
        ReceiverError::InvalidGuardianKeyRecovery
    );

    // Done.
    Ok(())
}
