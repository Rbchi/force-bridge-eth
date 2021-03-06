use crate::util::ckb_util::{
    get_sudt_type_script, handle_unconfirmed_headers, parse_cell, parse_main_raw_data,
    parse_uncle_raw_data, ETHSPVProofJson, EthWitness,
};
use crate::util::config::{DeployedContracts, ForceConfig, OutpointConf};
use crate::util::eth_proof_helper::Witness;
use crate::util::eth_util::convert_to_header_rlp;
use anyhow::{anyhow, bail, Result};
use ckb_sdk::constants::{MIN_SECP_CELL_CAPACITY, ONE_CKB};
use ckb_sdk::{GenesisInfo, HttpRpcClient};
use ckb_types::core::{BlockView, Capacity, DepType, ScriptHashType, TransactionView};
use ckb_types::packed::{HeaderVec, WitnessArgs};
use ckb_types::prelude::{Builder, Entity, Pack, Reader};
use ckb_types::{
    bytes::Bytes,
    packed::{self, Byte32, CellDep, CellOutput, OutPoint, Script},
};
use ethereum_types::H160;
use force_eth_types::eth_recipient_cell::{ETHAddress, ETHRecipientDataView};
use force_eth_types::generated::basic;
use force_eth_types::generated::basic::BytesVec;
use force_eth_types::generated::eth_bridge_lock_cell::ETHBridgeLockArgs;
use force_eth_types::generated::eth_bridge_type_cell::ETHBridgeTypeData;
use force_eth_types::generated::eth_header_cell::{
    ETHChain, ETHHeaderCellData, ETHHeaderInfo, ETHHeaderInfoReader, ETHLightClientWitness,
};
use force_sdk::cell_collector::{collect_sudt_amount, get_live_cell_by_typescript};
use force_sdk::indexer::{Cell, IndexerRpcClient};
use force_sdk::tx_helper::{sign, TxHelper};
use force_sdk::util::{get_live_cell_with_cache, send_tx_sync};
use log::info;
use secp256k1::SecretKey;
use shellexpand::tilde;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::ops::Add;
use web3::types::{Block, BlockHeader};

pub const MAIN_HEADER_CACHE_LIMIT: usize = 500;
pub const CONFIRM: usize = 10;
pub const UNCLE_HEADER_CACHE_LIMIT: usize = 10;

pub struct Generator {
    pub rpc_client: HttpRpcClient,
    pub indexer_client: IndexerRpcClient,
    genesis_info: GenesisInfo,
    pub deployed_contracts: DeployedContracts,
}

impl Generator {
    pub fn new(rpc_url: String, indexer_url: String, settings: DeployedContracts) -> Result<Self> {
        let mut rpc_client = HttpRpcClient::new(rpc_url);
        let indexer_client = IndexerRpcClient::new(indexer_url);
        let genesis_block: BlockView = rpc_client
            .get_block_by_number(0)
            .map_err(|err| anyhow!(err))?
            .ok_or_else(|| anyhow!("Can not get genesis block?"))?
            .into();
        let genesis_info = GenesisInfo::from_block(&genesis_block).map_err(|err| anyhow!(err))?;
        Ok(Self {
            rpc_client,
            indexer_client,
            genesis_info,
            deployed_contracts: settings,
        })
    }

    #[allow(clippy::mutable_key_type)]
    pub fn init_light_client_tx(
        &mut self,
        block: &Block<ethereum_types::H256>,
        _witness: &Witness,
        from_lockscript: Script,
        typescript: Script,
        lockscript: Script,
    ) -> Result<TransactionView> {
        let tx_fee: u64 = 500_000;
        let mut helper = TxHelper::default();

        let outpoints = vec![
            self.deployed_contracts.dag_merkle_roots.clone(),
            self.deployed_contracts
                .light_client_lockscript
                .outpoint
                .clone(),
            self.deployed_contracts
                .light_client_typescript
                .outpoint
                .clone(),
        ];
        self.add_cell_deps(&mut helper, outpoints)
            .map_err(|err| anyhow!(err))?;
        let output = CellOutput::new_builder()
            .capacity(Capacity::shannons(1000 * MIN_SECP_CELL_CAPACITY).pack())
            .build();
        let header_rlp = convert_to_header_rlp(block)?;
        let header_info = ETHHeaderInfo::new_builder()
            .header(hex::decode(header_rlp)?.into())
            .total_difficulty(block.total_difficulty.unwrap().as_u64().into())
            .hash(basic::Byte32::from_slice(block.hash.unwrap().as_bytes()).unwrap())
            .build();
        let main_chain_data: Vec<basic::Bytes> = vec![header_info.as_slice().to_vec().into()];

        // let proofs = build_merkle_proofs(&witness)?;
        let output_data = ETHHeaderCellData::new_builder()
            .headers(
                ETHChain::new_builder()
                    .main(BytesVec::new_builder().set(main_chain_data).build())
                    .build(),
            )
            // .merkle_proofs(MerkleProofVec::new_builder().set(vec![proofs]).build())
            .build()
            .as_bytes();
        helper.add_output(output.clone(), output_data);
        // add witness
        {
            let header_rlp = convert_to_header_rlp(block)?;
            let witness_data = ETHLightClientWitness::new_builder()
                .headers(
                    BytesVec::new_builder()
                        .set(vec![hex::decode(header_rlp)
                            .map_err(|err| anyhow!(err))?
                            .into()])
                        .build(),
                )
                .cell_dep_index_list(vec![0].into())
                .build();
            let witness_args = WitnessArgs::new_builder()
                .input_type(Some(witness_data.as_bytes()).pack())
                .build();
            helper.transaction = helper
                .transaction
                .as_advanced_builder()
                .set_witnesses(vec![witness_args.as_bytes().pack()])
                .build();
        }

        // build tx
        let tx = helper
            .supply_capacity(
                &mut self.rpc_client,
                &mut self.indexer_client,
                from_lockscript,
                &self.genesis_info,
                tx_fee,
            )
            .map_err(|err| anyhow!(err))?;
        let first_outpoint = tx
            .inputs()
            .get(0)
            .expect("should have input")
            .previous_output()
            .as_bytes();
        let typescript_args = first_outpoint.as_ref();
        let new_typescript = typescript.as_builder().args(typescript_args.pack()).build();

        let new_output = CellOutput::new_builder()
            .capacity(output.capacity())
            .type_(Some(new_typescript).pack())
            .lock(lockscript)
            .build();
        let mut new_outputs = tx.outputs().into_iter().collect::<Vec<_>>();
        new_outputs[0] = new_output;
        let tx = tx.as_advanced_builder().set_outputs(new_outputs).build();
        Ok(tx)
    }

    #[allow(clippy::mutable_key_type)]
    pub fn generate_eth_light_client_tx(
        &mut self,
        headers: &[Block<ethereum_types::H256>],
        cell: &Cell,
        _witness: &[Witness],
        un_confirmed_headers: &[BlockHeader],
        from_lockscript: Script,
    ) -> Result<TransactionView> {
        info!("generate eth light client tx.");
        let tx_fee: u64 = 500_000;
        let mut helper = TxHelper::default();

        let outpoints = vec![
            self.deployed_contracts.dag_merkle_roots.clone(),
            self.deployed_contracts
                .light_client_lockscript
                .outpoint
                .clone(),
            self.deployed_contracts
                .light_client_typescript
                .outpoint
                .clone(),
        ];
        self.add_cell_deps(&mut helper, outpoints)
            .map_err(|err| anyhow!(err))?;

        let mut live_cell_cache: HashMap<(OutPoint, bool), (CellOutput, Bytes)> =
            Default::default();
        let rpc_client = &mut self.rpc_client;
        let mut get_live_cell_fn = |out_point: OutPoint, with_data: bool| {
            get_live_cell_with_cache(&mut live_cell_cache, rpc_client, out_point, with_data)
                .map(|(output, _)| output)
        };
        helper
            .add_input(
                OutPoint::from(cell.clone().out_point),
                None,
                &mut get_live_cell_fn,
                &self.genesis_info,
                true,
            )
            .map_err(|err| anyhow!(err))?;
        {
            let cell_output = CellOutput::from(cell.clone().output);
            let output = CellOutput::new_builder()
                .lock(cell_output.lock())
                .type_(cell_output.type_())
                .build();
            let tip = &un_confirmed_headers[un_confirmed_headers.len() - 1];
            let input_cell_data = packed::Bytes::from(cell.clone().output_data).raw_data();
            let (mut unconfirmed, mut confirmed) = parse_main_raw_data(&input_cell_data)?;
            let mut uncle_raw_data = parse_uncle_raw_data(&input_cell_data)?;
            let header_infos;
            if tip.hash.unwrap() == headers[0].parent_hash {
                // the main chain is not reorg.
                if unconfirmed.len().add(headers.len()) > CONFIRM {
                    let mut idx = unconfirmed.len().add(headers.len()) - CONFIRM;
                    while idx > 0 {
                        let temp_data = unconfirmed[0];
                        ETHHeaderInfoReader::verify(&temp_data, false)
                            .map_err(|err| anyhow!(err))?;
                        let header_info_reader = ETHHeaderInfoReader::new_unchecked(&temp_data);
                        let hash = header_info_reader.hash().raw_data();
                        confirmed.push(hash);
                        unconfirmed.remove(0);
                        idx -= 1;
                    }
                }
                if confirmed.len().add(unconfirmed.len()).add(headers.len())
                    > MAIN_HEADER_CACHE_LIMIT
                {
                    let mut idx = confirmed.len().add(unconfirmed.len()).add(headers.len())
                        - MAIN_HEADER_CACHE_LIMIT;
                    while idx > 0 {
                        confirmed.remove(0);
                        idx -= 1;
                    }
                }

                let input_tail_raw = unconfirmed[unconfirmed.len() - 1];
                header_infos = handle_unconfirmed_headers(input_tail_raw, headers)?;
                for item in &header_infos {
                    unconfirmed.push(item.as_slice());
                }
                info!(
                    "main chain confirmed len: {:?}, un_confirmed len: {:?}",
                    confirmed.len(),
                    unconfirmed.len()
                );
            } else {
                // the main chain had been reorged.
                let mut idx = un_confirmed_headers.len() - 1;
                while idx > 0 {
                    let header = &un_confirmed_headers[idx - 1];
                    if header.hash.unwrap() == headers[0].parent_hash {
                        break;
                    }
                    idx -= 1;
                }
                // remove the item to uncle chain if the index >= idx
                for i in idx..un_confirmed_headers.len() {
                    if uncle_raw_data.len() == UNCLE_HEADER_CACHE_LIMIT {
                        uncle_raw_data.remove(0);
                    }
                    unconfirmed.remove(i);
                    uncle_raw_data.push(unconfirmed[i]);
                }

                let input_tail_raw = unconfirmed[idx - 1];
                header_infos = handle_unconfirmed_headers(input_tail_raw, headers)?;
                for item in &header_infos {
                    unconfirmed.push(item.as_slice());
                    if unconfirmed.len() > CONFIRM {
                        let temp_data = unconfirmed[0];
                        ETHHeaderInfoReader::verify(&temp_data, false)
                            .map_err(|err| anyhow!(err))?;
                        let header_info_reader = ETHHeaderInfoReader::new_unchecked(&temp_data);
                        let hash = header_info_reader.hash().raw_data();
                        confirmed.push(hash);
                        if confirmed.len() > MAIN_HEADER_CACHE_LIMIT {
                            confirmed.remove(0);
                        }
                        unconfirmed.remove(0);
                    }
                }
            }
            let mut main_chain_data: Vec<basic::Bytes> = vec![];
            for item in confirmed {
                main_chain_data.push(item.to_vec().into());
            }
            for item in unconfirmed {
                main_chain_data.push(item.to_vec().into());
            }
            let mut uncle_chain_data = vec![];
            for item in uncle_raw_data {
                uncle_chain_data.push(item.to_vec().into());
            }
            // Turn on this in later versions
            // let mut proofs: Vec<MerkleProof> = vec![];
            // for item in witness {
            //     let proof = build_merkle_proofs(&item)?;
            //     proofs.push(proof);
            // }

            let output_data = ETHHeaderCellData::new_builder()
                .headers(
                    ETHChain::new_builder()
                        .main(BytesVec::new_builder().set(main_chain_data).build())
                        .uncle(BytesVec::new_builder().set(uncle_chain_data).build())
                        .build(),
                )
                // .merkle_proofs(MerkleProofVec::new_builder().set(proofs).build())
                .build()
                .as_bytes();
            helper.add_output_with_auto_capacity(output, output_data);
        }

        {
            // add witness
            let mut headers_raw = vec![];
            for item in headers {
                let header_rlp = convert_to_header_rlp(item)?;
                headers_raw.push(basic::Bytes::from(
                    hex::decode(header_rlp).map_err(|err| anyhow!(err))?,
                ))
            }
            let witness_data = ETHLightClientWitness::new_builder()
                .headers(BytesVec::new_builder().set(headers_raw).build())
                .cell_dep_index_list(vec![0].into())
                .build();

            let witness_args = WitnessArgs::new_builder()
                .input_type(Some(witness_data.as_bytes()).pack())
                .build();
            helper.transaction = helper
                .transaction
                .as_advanced_builder()
                .set_witnesses(vec![witness_args.as_bytes().pack()])
                .build();
        }
        // build tx
        let tx = helper
            .supply_capacity(
                &mut self.rpc_client,
                &mut self.indexer_client,
                from_lockscript,
                &self.genesis_info,
                tx_fee,
            )
            .map_err(|err| anyhow!(err))?;

        Ok(tx)
    }

    #[allow(clippy::mutable_key_type)]
    pub fn generate_eth_spv_tx(
        &mut self,
        config_path: String,
        from_lockscript: Script,
        eth_proof: &ETHSPVProofJson,
    ) -> Result<TransactionView> {
        let tx_fee: u64 = ONE_CKB / 2;
        let mut helper = TxHelper::default();
        let config_path = tilde(config_path.as_str()).into_owned();
        let force_cli_config = ForceConfig::new(config_path.as_str())?;
        let deployed_contracts = force_cli_config
            .deployed_contracts
            .as_ref()
            .ok_or_else(|| anyhow!("contracts should be deployed"))?;
        // add cell deps.
        {
            let cell_script = parse_cell(
                deployed_contracts
                    .light_client_cell_script
                    .cell_script
                    .as_str(),
            )?;
            let cell = get_live_cell_by_typescript(&mut self.indexer_client, cell_script)
                .map_err(|err| anyhow!(err))?
                .ok_or_else(|| anyhow!("no cell found for cell dep"))?;
            let mut builder = helper.transaction.as_advanced_builder();
            builder = builder.cell_dep(
                CellDep::new_builder()
                    .out_point(cell.out_point.into())
                    .dep_type(DepType::Code.into())
                    .build(),
            );
            helper.transaction = builder.build();

            let outpoints = vec![
                self.deployed_contracts.bridge_lockscript.outpoint.clone(),
                self.deployed_contracts.bridge_typescript.outpoint.clone(),
                self.deployed_contracts.sudt.outpoint.clone(),
            ];
            self.add_cell_deps(&mut helper, outpoints)
                .map_err(|err| anyhow!(err))?;
        }

        let lockscript_code_hash =
            hex::decode(&self.deployed_contracts.bridge_lockscript.code_hash)?;
        use force_eth_types::generated::basic::ETHAddress;
        let args = ETHBridgeLockArgs::new_builder()
            .eth_token_address(
                ETHAddress::from_slice(&eth_proof.token.as_bytes()).map_err(|err| anyhow!(err))?,
            )
            .eth_contract_address(
                ETHAddress::from_slice(&eth_proof.eth_address.as_bytes())
                    .map_err(|err| anyhow!(err))?,
            )
            .build();
        let lockscript = Script::new_builder()
            .code_hash(Byte32::from_slice(&lockscript_code_hash)?)
            .hash_type(ScriptHashType::Data.into())
            .args(args.as_bytes().pack())
            .build();

        // input bridge cells
        let rpc_client = &mut self.rpc_client;
        let mut live_cell_cache: HashMap<(OutPoint, bool), (CellOutput, Bytes)> =
            Default::default();
        let mut get_live_cell_fn = |out_point: OutPoint, with_data: bool| {
            get_live_cell_with_cache(&mut live_cell_cache, rpc_client, out_point, with_data)
                .map(|(output, _)| output)
        };
        let outpoint = OutPoint::from_slice(&eth_proof.replay_resist_outpoint)
            .expect("replay resist outpoint in lock event is invalid");
        helper
            .add_input(
                outpoint.clone(),
                None,
                &mut get_live_cell_fn,
                &self.genesis_info,
                true,
            )
            .map_err(|err| anyhow!(err))?;

        let (bridge_cell, bridge_cell_data) =
            get_live_cell_with_cache(&mut live_cell_cache, &mut self.rpc_client, outpoint, true)
                .expect("outpoint not exists");
        let owner_lock_script = ETHBridgeTypeData::from_slice(bridge_cell_data.as_ref())
            .expect("invalid bridge data")
            .owner_lock_script();
        if owner_lock_script.raw_data() != from_lockscript.as_bytes() {
            bail!("only support use bridge cell we created as lock outpoint");
        }
        // 1 xt cells
        {
            let recipient_lockscript = Script::from_slice(&eth_proof.recipient_lockscript).unwrap();

            let sudt_typescript_code_hash = hex::decode(&self.deployed_contracts.sudt.code_hash)?;
            let sudt_typescript = Script::new_builder()
                .code_hash(Byte32::from_slice(&sudt_typescript_code_hash)?)
                .hash_type(ScriptHashType::Data.into())
                .args(lockscript.calc_script_hash().as_bytes().pack())
                .build();

            // recipient
            let sudt_user_output = CellOutput::new_builder()
                .type_(Some(sudt_typescript.clone()).pack())
                .lock(recipient_lockscript)
                .build();
            let mut to_user_amount_data = (eth_proof.lock_amount - eth_proof.bridge_fee)
                .to_le_bytes()
                .to_vec();
            to_user_amount_data.extend(eth_proof.sudt_extra_data.clone());
            helper.add_output_with_auto_capacity(sudt_user_output, to_user_amount_data.into());
            // fee
            if eth_proof.bridge_fee != 0 {
                let sudt_fee_output = CellOutput::new_builder()
                    .type_(Some(sudt_typescript).pack())
                    .lock(from_lockscript.clone())
                    .build();
                helper.add_output_with_auto_capacity(
                    sudt_fee_output,
                    eth_proof.bridge_fee.to_le_bytes().to_vec().into(),
                );
            }
        }
        // 2 create new bridge cell for user
        helper.add_output(bridge_cell, bridge_cell_data);
        // add witness
        {
            let witness = EthWitness {
                cell_dep_index_list: vec![0],
                spv_proof: eth_proof.clone(),
            }
            .as_bytes();
            helper.transaction = helper
                .transaction
                .as_advanced_builder()
                .witness(witness.pack())
                .build();
        }
        // build tx
        let tx = helper
            .supply_capacity(
                &mut self.rpc_client,
                &mut self.indexer_client,
                from_lockscript,
                &self.genesis_info,
                tx_fee,
            )
            .map_err(|err| anyhow!(err))?;
        Ok(tx)
    }

    fn add_cell_deps(
        &mut self,
        helper: &mut TxHelper,
        outpoints: Vec<OutpointConf>,
    ) -> Result<(), String> {
        let mut builder = helper.transaction.as_advanced_builder();
        for conf in outpoints {
            let outpoint = OutPoint::new_builder()
                .tx_hash(
                    Byte32::from_slice(
                        &hex::decode(conf.tx_hash)
                            .map_err(|e| format!("invalid OutpointConf config. err: {}", e))?,
                    )
                    .map_err(|e| format!("invalid OutpointConf config. err: {}", e))?,
                )
                .index(conf.index.pack())
                .build();
            builder = builder.cell_dep(
                CellDep::new_builder()
                    .out_point(outpoint)
                    .dep_type(conf.dep_type.into())
                    .build(),
            );
        }
        helper.transaction = builder.build();
        Ok(())
    }

    pub fn get_ckb_cell(
        &mut self,
        // helper: &mut TxHelper,
        cell_typescript: Script,
        // add_to_input: bool,
    ) -> Result<(CellOutput, Bytes), String> {
        // let genesis_info = self.genesis_info.clone();
        let cell = get_live_cell_by_typescript(&mut self.indexer_client, cell_typescript)?
            .ok_or("cell not found")?;
        let ckb_cell = CellOutput::from(cell.output);
        let ckb_cell_data = packed::Bytes::from(cell.output_data).raw_data();
        // if add_to_input {
        //     let mut get_live_cell_fn = |out_point: OutPoint, with_data: bool| {
        //         get_live_cell(&mut self.rpc_client, out_point, with_data).map(|(output, _)| output)
        //     };
        //
        //     helper.add_input(
        //         cell.out_point.into(),
        //         None,
        //         &mut get_live_cell_fn,
        //         &genesis_info,
        //         true,
        //     )?;
        // }
        Ok((ckb_cell, ckb_cell_data))
    }
    pub fn get_ckb_headers(&mut self, block_numbers: Vec<u64>) -> Result<Vec<u8>> {
        let mut mol_header_vec: Vec<packed::Header> = Default::default();
        for number in block_numbers {
            let header = self
                .rpc_client
                .get_header_by_number(number)
                .map_err(|e| anyhow::anyhow!("failed to get header: {}", e))?
                .ok_or_else(|| anyhow::anyhow!("failed to get header which is none"))?;

            mol_header_vec.push(header.inner.into());
        }
        let mol_headers = HeaderVec::new_builder().set(mol_header_vec).build();
        Ok(Vec::from(mol_headers.as_slice()))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_bridge_cell(
        &mut self,
        tx_fee: u64,
        capacity: u64,
        from_lockscript: Script,
        bridge_typescript: Script,
        bridge_lockscript: Script,
        bridge_fee: u128,
        cell_num: usize,
    ) -> Result<TransactionView> {
        let mut tx_helper = TxHelper::default();
        // add cell deps
        let outpoints = vec![
            self.deployed_contracts.bridge_lockscript.outpoint.clone(),
            self.deployed_contracts.bridge_typescript.outpoint.clone(),
        ];
        self.add_cell_deps(&mut tx_helper, outpoints)
            .map_err(|err| anyhow!(err))?;
        // build bridge data
        let bridge_data = ETHBridgeTypeData::new_builder()
            .owner_lock_script(from_lockscript.as_slice().to_vec().into())
            .fee(bridge_fee.into())
            .build();
        // build output
        let output = CellOutput::new_builder()
            .capacity(capacity.pack())
            .type_(Some(bridge_typescript).pack())
            .lock(bridge_lockscript)
            .build();
        for _ in 0..cell_num {
            tx_helper.add_output(output.clone(), bridge_data.as_bytes());
        }
        // build tx
        let tx = tx_helper
            .supply_capacity(
                &mut self.rpc_client,
                &mut self.indexer_client,
                from_lockscript,
                &self.genesis_info,
                tx_fee,
            )
            .map_err(|err| anyhow!(err))?;
        Ok(tx)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn burn(
        &mut self,
        tx_fee: u64,
        from_lockscript: Script,
        unlock_fee: u128,
        burn_sudt_amount: u128,
        token_addr: H160,
        lock_contract_addr: H160,
        eth_receiver_addr: H160,
    ) -> Result<TransactionView> {
        let mut helper = TxHelper::default();

        // add cellDeps
        {
            let mut outpoints = vec![
                self.deployed_contracts.bridge_lockscript.outpoint.clone(),
                self.deployed_contracts
                    .recipient_typescript
                    .outpoint
                    .clone(),
                self.deployed_contracts.sudt.outpoint.clone(),
            ];
            // add pw_lock deps
            outpoints.extend(self.deployed_contracts.pw_locks.inner.clone());
            self.add_cell_deps(&mut helper, outpoints)
                .map_err(|err| anyhow!(err))?;
        }

        let sudt_typescript = get_sudt_type_script(
            &self.deployed_contracts.bridge_lockscript.code_hash,
            &self.deployed_contracts.sudt.code_hash,
            token_addr,
            lock_contract_addr,
        )?;

        // gen output of eth_recipient cell
        {
            let mut eth_bridge_lock_hash = [0u8; 32];
            eth_bridge_lock_hash.copy_from_slice(
                &hex::decode(&self.deployed_contracts.bridge_lockscript.code_hash)
                    .map_err(|err| anyhow!(err))?,
            );
            let eth_recipient_data = ETHRecipientDataView {
                eth_recipient_address: ETHAddress::try_from(eth_receiver_addr.as_bytes().to_vec())
                    .map_err(|err| anyhow!(err))?,
                eth_token_address: ETHAddress::try_from(token_addr.as_bytes().to_vec())
                    .map_err(|err| anyhow!(err))?,
                eth_lock_contract_address: ETHAddress::try_from(
                    lock_contract_addr.as_bytes().to_vec(),
                )
                .map_err(|err| anyhow!(err))?,
                eth_bridge_lock_hash,
                token_amount: burn_sudt_amount,
                fee: unlock_fee,
            };

            log::info!(
                "tx fee: {} burn amount : {}",
                eth_recipient_data.fee,
                eth_recipient_data.token_amount
            );

            let mol_eth_recipient_data = eth_recipient_data
                .as_molecule_data()
                .map_err(|err| anyhow!(err))?;
            let recipient_typescript_code_hash =
                hex::decode(&self.deployed_contracts.recipient_typescript.code_hash)
                    .map_err(|err| anyhow!(err))?;

            let recipient_typescript: Script = Script::new_builder()
                .code_hash(Byte32::from_slice(&recipient_typescript_code_hash)?)
                .hash_type(ScriptHashType::Data.into())
                .build();

            let eth_recipient_output = CellOutput::new_builder()
                .lock(from_lockscript.clone())
                .type_(Some(recipient_typescript).pack())
                .build();
            helper.add_output_with_auto_capacity(eth_recipient_output, mol_eth_recipient_data);
        }

        helper
            .supply_sudt(
                &mut self.rpc_client,
                &mut self.indexer_client,
                from_lockscript.clone(),
                &self.genesis_info,
                burn_sudt_amount,
                sudt_typescript,
            )
            .map_err(|err| anyhow!(err))?;

        // build tx
        let tx = helper
            .supply_capacity(
                &mut self.rpc_client,
                &mut self.indexer_client,
                from_lockscript,
                &self.genesis_info,
                tx_fee,
            )
            .map_err(|err| anyhow!(err))?;
        Ok(tx)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn transfer_sudt(
        &mut self,
        lock_contract_addr: H160,
        token_addr: H160,
        from_lockscript: Script,
        to_lockscript: Script,
        sudt_amount: u128,
        ckb_amount: u64,
        tx_fee: u64,
    ) -> Result<TransactionView> {
        let mut helper = TxHelper::default();

        // add cellDeps
        let outpoints = vec![
            self.deployed_contracts.bridge_lockscript.outpoint.clone(),
            self.deployed_contracts.sudt.outpoint.clone(),
        ];
        self.add_cell_deps(&mut helper, outpoints)
            .map_err(|err| anyhow!(err))?;

        let sudt_typescript = get_sudt_type_script(
            &self.deployed_contracts.bridge_lockscript.code_hash,
            &self.deployed_contracts.sudt.code_hash,
            token_addr,
            lock_contract_addr,
        )?;

        let sudt_output = CellOutput::new_builder()
            .capacity(Capacity::shannons(ckb_amount).pack())
            .type_(Some(sudt_typescript.clone()).pack())
            .lock(to_lockscript)
            .build();

        helper.add_output(sudt_output, sudt_amount.to_le_bytes().to_vec().into());

        helper
            .supply_sudt(
                &mut self.rpc_client,
                &mut self.indexer_client,
                from_lockscript.clone(),
                &self.genesis_info,
                sudt_amount,
                sudt_typescript,
            )
            .map_err(|err| anyhow!(err))?;

        // add signature to pay tx fee
        let tx = helper
            .supply_capacity(
                &mut self.rpc_client,
                &mut self.indexer_client,
                from_lockscript,
                &self.genesis_info,
                tx_fee,
            )
            .map_err(|err| anyhow!(err))?;
        Ok(tx)
    }

    pub fn get_sudt_balance(
        &mut self,
        addr_lockscript: Script,
        token_addr: H160,
        lock_contract_addr: H160,
    ) -> Result<u128> {
        let sudt_typescript = get_sudt_type_script(
            &self.deployed_contracts.bridge_lockscript.code_hash,
            &self.deployed_contracts.sudt.code_hash,
            token_addr,
            lock_contract_addr,
        )?;
        collect_sudt_amount(&mut self.indexer_client, addr_lockscript, sudt_typescript)
            .map_err(|err| anyhow!(err))
    }

    pub async fn sign_and_send_transaction(
        &mut self,
        unsigned_tx: TransactionView,
        from_privkey: SecretKey,
    ) -> Result<String> {
        let tx = sign(unsigned_tx, &mut self.rpc_client, &from_privkey)
            .map_err(|e| anyhow!("failed to sign tx : {}", e))?;
        log::info!(
            "tx: \n{}",
            serde_json::to_string_pretty(&ckb_jsonrpc_types::TransactionView::from(tx.clone()))?
        );
        send_tx_sync(&mut self.rpc_client, &tx, 60)
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(hex::encode(tx.hash().as_slice()))
    }
}
