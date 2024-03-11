use crate::logic::{
    mocks::mock_external::MockedExternal, ProtocolVersion, ReturnData, VMContext, VMOutcome,
};
use crate::runner::VMKindExt;
use crate::ContractCode;
use unc_parameters::vm::{ContractPrepareVersion, VMKind};
use unc_parameters::{RuntimeConfig, RuntimeConfigStore, RuntimeFeesConfig};
use unc_primitives_core::types::Gas;
use unc_primitives_core::version::ProtocolFeature;
use std::{collections::HashSet, fmt::Write, sync::Arc};

pub(crate) fn test_builder() -> TestBuilder {
    let context = VMContext {
        current_account_id: "alice".parse().unwrap(),
        signer_account_id: "bob".parse().unwrap(),
        signer_account_pk: vec![0, 1, 2],
        predecessor_account_id: "carol".parse().unwrap(),
        input: Vec::new(),
        block_height: 10,
        block_timestamp: 42,
        epoch_height: 1,
        account_balance: 2u128,
        account_locked_balance: 0,
        storage_usage: 12,
        attached_deposit: 2u128,
        prepaid_gas: 10_u64.pow(14),
        random_seed: vec![0, 1, 2],
        view_config: None,
        output_data_receivers: vec![],
    };
    let mut skip = HashSet::new();
    if cfg!(not(target_arch = "x86_64")) {
        skip.insert(VMKind::Wasmer0);
        skip.insert(VMKind::Wasmer2);
        skip.insert(VMKind::NearVm);
    }
    TestBuilder {
        code: ContractCode::new(Vec::new(), None),
        context,
        method: "main".to_string(),
        protocol_versions: vec![u32::MAX],
        skip,
        opaque_error: false,
        opaque_outcome: false,
    }
}

pub(crate) struct TestBuilder {
    code: ContractCode,
    context: VMContext,
    protocol_versions: Vec<ProtocolVersion>,
    method: String,
    skip: HashSet<VMKind>,
    opaque_error: bool,
    opaque_outcome: bool,
}

impl TestBuilder {
    pub(crate) fn wat(mut self, wat: &str) -> Self {
        let wasm = wat::parse_str(wat)
            .unwrap_or_else(|err| panic!("failed to parse input wasm: {err}\n{wat}"));
        self.code = ContractCode::new(wasm, None);
        self
    }

    pub(crate) fn wasm(mut self, wasm: &[u8]) -> Self {
        self.code = ContractCode::new(wasm.to_vec(), None);
        self
    }

    #[allow(dead_code)]
    pub(crate) fn get_wasm(&self) -> &[u8] {
        self.code.code()
    }

    pub(crate) fn method(mut self, method: &str) -> Self {
        self.method = method.to_string();
        self
    }

    pub(crate) fn gas(mut self, gas: Gas) -> Self {
        self.context.prepaid_gas = gas;
        self
    }

    pub(crate) fn opaque_error(mut self) -> Self {
        self.opaque_error = true;
        self
    }

    pub(crate) fn opaque_outcome(mut self) -> Self {
        self.opaque_outcome = true;
        self
    }

    // We only test trapping tests on Wasmer, as of version 0.17, when tests executed in parallel,
    // Wasmer signal handlers may catch signals thrown from the Wasmtime, and produce fake failing tests.
    pub(crate) fn skip_wasmtime(mut self) -> Self {
        self.skip.insert(VMKind::Wasmtime);
        self
    }

    pub(crate) fn skip_wasmer0(mut self) -> Self {
        self.skip.insert(VMKind::Wasmer0);
        self
    }

    pub(crate) fn skip_wasmer2(mut self) -> Self {
        self.skip.insert(VMKind::Wasmer2);
        self
    }

    pub(crate) fn skip_unc_vm(mut self) -> Self {
        self.skip.insert(VMKind::NearVm);
        self
    }

    #[allow(dead_code)]
    pub(crate) fn only_wasmtime(self) -> Self {
        self.skip_wasmer0().skip_wasmer2().skip_unc_vm()
    }

    pub(crate) fn only_wasmer0(self) -> Self {
        self.skip_wasmer2().skip_unc_vm().skip_wasmtime()
    }

    #[allow(dead_code)]
    pub(crate) fn only_wasmer2(self) -> Self {
        self.skip_wasmer0().skip_unc_vm().skip_wasmtime()
    }

    pub(crate) fn only_unc_vm(self) -> Self {
        self.skip_wasmer0().skip_wasmer2().skip_wasmtime()
    }

    /// Add additional protocol features to this test.
    ///
    /// Tricky. Given `[feat1, feat2, feat3]`, this will run *four* tests for
    /// protocol versions `[feat1 - 1, feat2 - 1, feat3 - 1, PROTOCOL_VERSION]`.
    ///
    /// When using this method with `n` features, be sure to pass `n + 1`
    /// expectations to the `expects` method. For nightly features, you can
    /// `cfg` the relevant features and expect.
    pub(crate) fn protocol_features(
        mut self,
        protocol_features: &'static [ProtocolFeature],
    ) -> Self {
        for feat in protocol_features {
            self = self.protocol_version(feat.protocol_version() - 1);
        }
        self
    }

    /// Add a protocol version to test.
    pub(crate) fn protocol_version(mut self, protocol_version: ProtocolVersion) -> Self {
        self.protocol_versions.push(protocol_version - 1);
        self
    }

    pub(crate) fn only_protocol_versions(
        mut self,
        protocol_versions: Vec<ProtocolVersion>,
    ) -> Self {
        self.protocol_versions = protocol_versions;
        self
    }

    #[track_caller]
    pub(crate) fn expect(self, want: &expect_test::Expect) {
        self.expects(std::iter::once(want))
    }

    pub(crate) fn configs(&self) -> impl Iterator<Item = Arc<RuntimeConfig>> {
        let runtime_config_store = RuntimeConfigStore::new(None);
        self.protocol_versions
            .clone()
            .into_iter()
            .map(move |pv| Arc::clone(runtime_config_store.get_config(pv)))
    }

    #[track_caller]
    pub(crate) fn expects<'a, I>(mut self, wants: I)
    where
        I: IntoIterator<Item = &'a expect_test::Expect>,
        I::IntoIter: ExactSizeIterator,
    {
        self.protocol_versions.sort();
        let runtime_config_store = RuntimeConfigStore::new(None);
        let wants = wants.into_iter();
        assert_eq!(
            wants.len(),
            self.protocol_versions.len(),
            "specified {} protocol versions but only {} expectation",
            self.protocol_versions.len(),
            wants.len(),
        );

        for (want, &protocol_version) in wants.zip(&self.protocol_versions) {
            let mut results = vec![];
            for vm_kind in [VMKind::NearVm, VMKind::Wasmer2, VMKind::Wasmer0, VMKind::Wasmtime] {
                if self.skip.contains(&vm_kind) {
                    continue;
                }

                let runtime_config = runtime_config_store.get_config(protocol_version);

                // NearVM includes a different contract preparation algorithm, that is not supported on old protocol versions
                if vm_kind == VMKind::NearVm
                    && runtime_config.wasm_config.limit_config.contract_prepare_version
                        != ContractPrepareVersion::V2
                {
                    continue;
                }

                let mut fake_external = MockedExternal::new();
                let config = runtime_config.wasm_config.clone();
                let fees = RuntimeFeesConfig::test();
                let context = self.context.clone();

                let promise_results = vec![];

                let runtime = vm_kind.runtime(config).expect("runtime has not been compiled");
                println!("Running {:?} for protocol version {}", vm_kind, protocol_version);
                let outcome = runtime
                    .run(
                        &self.code,
                        &self.method,
                        &mut fake_external,
                        context,
                        &fees,
                        &promise_results,
                        None,
                    )
                    .expect("execution failed");

                let mut got = String::new();

                if !self.opaque_outcome {
                    fmt_outcome_without_abort(&outcome, &mut got).unwrap();
                    writeln!(&mut got).unwrap();
                }

                if let Some(err) = outcome.aborted {
                    let err_str = err.to_string();
                    assert!(err_str.len() < 1000, "errors should be bounded in size to prevent abuse via exhausting the storage space");
                    if self.opaque_error {
                        writeln!(&mut got, "Err: ...").unwrap();
                    } else {
                        writeln!(&mut got, "Err: {err_str}").unwrap();
                    }
                };

                results.push((vm_kind, got));
            }

            if !results.is_empty() {
                want.assert_eq(&results[0].1);
                for i in 1..results.len() {
                    if results[i].1 != results[0].1 {
                        panic!(
                            "Inconsistent VM Output:\n{:?}:\n{}\n\n{:?}:\n{}",
                            results[0].0, results[0].1, results[i].0, results[i].1
                        )
                    }
                }
            }
        }
    }
}

fn fmt_outcome_without_abort(
    outcome: &VMOutcome,
    out: &mut dyn std::fmt::Write,
) -> std::fmt::Result {
    let return_data_str = match &outcome.return_data {
        ReturnData::None => "None".to_string(),
        ReturnData::ReceiptIndex(_) => "Receipt".to_string(),
        ReturnData::Value(v) => format!("Value [{} bytes]", v.len()),
    };
    write!(
        out,
        "VMOutcome: balance {} storage_usage {} return data {} burnt gas {} used gas {}",
        outcome.balance,
        outcome.storage_usage,
        return_data_str,
        outcome.burnt_gas,
        outcome.used_gas
    )?;
    Ok(())
}
