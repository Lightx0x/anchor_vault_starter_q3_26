use {
    anchor_lang::{
        prelude::Pubkey,
        solana_program::{instruction::Instruction, system_program},
        InstructionData, ToAccountMetas,
    },
    litesvm::LiteSVM,
    q3_26_vault::{STATE_SEED, VAULT_SEED},
    solana_keypair::Keypair,
    solana_message::{Message, VersionedMessage},
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
};

const DEPOSIT_LAMPORTS: u64 = 500_000_000;
const WITHDRAW_LAMPORTS: u64 = 100_000_000;

fn send(svm: &mut LiteSVM, payer: &Keypair, ix: Instruction) {
    // Fresh blockhash per tx, so re-sending an identical instruction (the
    // re-initialize below) doesn't collide on signature as AlreadyProcessed.
    svm.expire_blockhash();
    let blockhash = svm.latest_blockhash();
    let msg = Message::new_with_blockhash(&[ix], Some(&payer.pubkey()), &blockhash);
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[payer]).unwrap();
    svm.send_transaction(tx).unwrap_or_else(|err| {
        panic!("tx failed: {err:?}\nlogs: {logs:#?}", logs = err.meta.logs);
    });
}

fn send_expect_err(svm: &mut LiteSVM, payer: &Keypair, ix: Instruction) -> String {
    svm.expire_blockhash();
    let blockhash = svm.latest_blockhash();
    let msg = Message::new_with_blockhash(&[ix], Some(&payer.pubkey()), &blockhash);
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[payer]).unwrap();
    match svm.send_transaction(tx) {
        Ok(_) => panic!("expected the transaction to fail, but it succeeded"),
        Err(err) => format!("{:?}", err.err),
    }
}

#[test]
fn test() {
    let program_id = q3_26_vault::id();
    let user = Keypair::new();
    let (vault_state, _) =
        Pubkey::find_program_address(&[STATE_SEED, user.pubkey().as_ref()], &program_id);
    let (vault, _) =
        Pubkey::find_program_address(&[VAULT_SEED, user.pubkey().as_ref()], &program_id);

    let mut svm = LiteSVM::new();
    let bytes = include_bytes!(concat!(
        env!("CARGO_TARGET_TMPDIR"),
        "/../deploy/q3_26_vault.so"
    ));
    svm.add_program(program_id, bytes).unwrap();
    svm.airdrop(&user.pubkey(), 2_000_000_000).unwrap();

    let attacker = Keypair::new();
    svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();

    let init_ix = Instruction::new_with_bytes(
        program_id,
        &q3_26_vault::instruction::Initialize {}.data(),
        q3_26_vault::accounts::Initialize {
            user: user.pubkey(),
            vault_state,
            vault,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
    );

    send(&mut svm, &user, init_ix.clone());

    let rent_exempt = svm.minimum_balance_for_rent_exemption(0);
    assert!(
        svm.get_account(&vault_state).is_some(),
        "vault_state should exist after initialize"
    );
    assert_eq!(
        svm.get_balance(&vault).unwrap(),
        rent_exempt,
        "vault should be rent-exempt after initialize"
    );

    send(
        &mut svm,
        &user,
        Instruction::new_with_bytes(
            program_id,
            &q3_26_vault::instruction::Deposit {
                amount: DEPOSIT_LAMPORTS,
            }
            .data(),
            q3_26_vault::accounts::Deposit {
                user: user.pubkey(),
                vault_state,
                vault,
                system_program: system_program::ID,
            }
            .to_account_metas(None),
        ),
    );

    assert_eq!(
        svm.get_balance(&vault).unwrap(),
        rent_exempt + DEPOSIT_LAMPORTS,
        "vault should increase by the deposit"
    );

    // Nobody else can drain a funded vault. The attacker signs in the `user`
    // slot, so the Signer check passes, but both PDAs then have to re-derive
    // from *their* key and no longer match the accounts they had to pass.
    let err = send_expect_err(
        &mut svm,
        &attacker,
        Instruction::new_with_bytes(
            program_id,
            &q3_26_vault::instruction::Withdraw {
                amount: DEPOSIT_LAMPORTS,
            }
            .data(),
            q3_26_vault::accounts::Withdraw {
                user: attacker.pubkey(),
                vault_state,
                vault,
                system_program: system_program::ID,
            }
            .to_account_metas(None),
        ),
    );
    assert!(
        err.contains("Custom(2006)"),
        "an unauthorized withdraw should violate the seeds constraint, got: {err}"
    );
    assert_eq!(
        svm.get_balance(&vault).unwrap(),
        rent_exempt + DEPOSIT_LAMPORTS,
        "a rejected withdraw must not move lamports"
    );

    let withdraw_ix = |amount: u64| {
        Instruction::new_with_bytes(
            program_id,
            &q3_26_vault::instruction::Withdraw { amount }.data(),
            q3_26_vault::accounts::Withdraw {
                user: user.pubkey(),
                vault_state,
                vault,
                system_program: system_program::ID,
            }
            .to_account_metas(None),
        )
    };

    send(&mut svm, &user, withdraw_ix(WITHDRAW_LAMPORTS));

    assert_eq!(
        svm.get_balance(&vault).unwrap(),
        rent_exempt + DEPOSIT_LAMPORTS - WITHDRAW_LAMPORTS,
        "vault should decrease by the withdraw"
    );

    // The rent floor, pinned from both sides. One lamport past the free balance
    // must be refused by the program (not the runtime) and must move nothing.
    let free = svm.get_balance(&vault).unwrap() - rent_exempt;

    let err = send_expect_err(&mut svm, &user, withdraw_ix(free + 1));
    assert!(
        err.contains("Custom(6001)"),
        "one lamport past the floor should be InsufficientFunds, got: {err}"
    );
    assert_eq!(
        svm.get_balance(&vault).unwrap(),
        rent_exempt + free,
        "a rejected withdraw must not move lamports"
    );

    let err = send_expect_err(&mut svm, &user, withdraw_ix(0));
    assert!(
        err.contains("Custom(6000)"),
        "a zero withdraw should be InvalidAmount, got: {err}"
    );

    // ...and draining to exactly the floor is allowed.
    send(&mut svm, &user, withdraw_ix(free));
    assert_eq!(
        svm.get_balance(&vault).unwrap(),
        rent_exempt,
        "draining exactly to the floor must be allowed"
    );

    send(
        &mut svm,
        &user,
        Instruction::new_with_bytes(
            program_id,
            &q3_26_vault::instruction::Close {}.data(),
            q3_26_vault::accounts::Close {
                user: user.pubkey(),
                vault_state,
                vault,
                system_program: system_program::ID,
            }
            .to_account_metas(None),
        ),
    );

    assert!(
        svm.get_account(&vault_state).is_none(),
        "vault_state should be closed"
    );
    assert_eq!(
        svm.get_balance(&vault).unwrap_or(0),
        0,
        "vault should be empty"
    );

    // `close = user` exists so the user can start over. Prove the lockout is gone.
    send(&mut svm, &user, init_ix);
    assert!(
        svm.get_account(&vault_state).is_some(),
        "re-initialize must succeed after close"
    );
    assert_eq!(
        svm.get_balance(&vault).unwrap(),
        rent_exempt,
        "re-initialized vault should be rent-exempt again"
    );
}
