use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::read_to_string;
use std::iter::zip;
use std::ops::DerefMut;

use anyhow::{anyhow, Context, Result};
use cairo_lang_compiler::db::RootDatabase;
use cairo_lang_defs::db::DefsGroup;
use cairo_lang_defs::ids::{ModuleId, ModuleItemId};
use cairo_lang_filesystem::db::FilesGroup;
use cairo_lang_filesystem::ids::{CrateId, CrateLongId};
use cairo_lang_semantic::db::SemanticGroup;
use cairo_lang_starknet::abi;
use cairo_lang_starknet::contract::{find_contracts, ContractDeclaration};
use cairo_lang_starknet::contract_class::{compile_prepared_db, ContractClass};
use cairo_lang_starknet::plugin::aux_data::StarkNetContractAuxData;
use cairo_lang_utils::UpcastMut;
use camino::Utf8PathBuf;
use convert_case::{Case, Casing};
use dojo_world::manifest::{
    Class, ComputedValueEntrypoint, DojoContract, DojoModel, Manifest, ManifestMethods,
    BASE_CONTRACT_NAME, EXECUTOR_CONTRACT_NAME, WORLD_CONTRACT_NAME,
};
use itertools::Itertools;
use scarb::compiler::helpers::{build_compiler_config, collect_main_crate_ids};
use scarb::compiler::{CompilationUnit, Compiler};
use scarb::core::{PackageName, TargetKind, Workspace};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use starknet::core::types::contract::SierraClass;
use starknet::core::types::FieldElement;
use tracing::{debug, trace, trace_span};

use crate::inline_macros::utils::{SYSTEM_READS, SYSTEM_WRITES};
use crate::plugin::{ComputedValuesAuxData, DojoAuxData};
use crate::semantics::utils::find_module_rw;

const CAIRO_PATH_SEPARATOR: &str = "::";

#[cfg(test)]
#[path = "compiler_test.rs"]
mod test;

pub const MANIFESTS_DIR: &str = "manifests";
pub const BASE_DIR: &str = "base";
pub const DEPLOYMENTS_DIR: &str = "deployments";
pub const CONTRACTS_DIR: &str = "contracts";
pub const MODELS_DIR: &str = "models";

pub struct DojoCompiler;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Props {
    pub build_external_contracts: Option<Vec<ContractSelector>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractSelector(String);

impl ContractSelector {
    fn package(&self) -> PackageName {
        let parts = self.0.split_once(CAIRO_PATH_SEPARATOR).unwrap_or((self.0.as_str(), ""));
        PackageName::new(parts.0)
    }

    fn full_path(&self) -> String {
        self.0.clone()
    }
}

impl Compiler for DojoCompiler {
    fn target_kind(&self) -> TargetKind {
        TargetKind::new("dojo")
    }

    fn compile(
        &self,
        unit: CompilationUnit,
        db: &mut RootDatabase,
        ws: &Workspace<'_>,
    ) -> Result<()> {
        let props: Props = unit.target().props()?;
        let target_dir = unit.target_dir(ws);
        let compiler_config = build_compiler_config(&unit, ws);

        let mut main_crate_ids = collect_main_crate_ids(&unit, db);
        let core_crate_ids: Vec<CrateId> = collect_core_crate_ids(db);
        main_crate_ids.extend(core_crate_ids);

        let contracts = find_project_contracts(
            db.upcast_mut(),
            main_crate_ids.clone(),
            props.build_external_contracts.clone(),
        )?;

        let contract_paths = contracts
            .iter()
            .map(|decl| decl.module_id().full_path(db.upcast_mut()))
            .collect::<Vec<_>>();
        trace!(contracts = ?contract_paths);

        let contracts = contracts.iter().collect::<Vec<_>>();

        let classes = {
            let _ = trace_span!("compile_starknet").enter();
            compile_prepared_db(db, &contracts, compiler_config)?
        };

        // (contract name, class hash)
        let mut compiled_classes: HashMap<SmolStr, (FieldElement, Option<abi::Contract>)> =
            HashMap::new();

        for (decl, class) in zip(contracts, classes) {
            let contract_full_path = decl.module_id().full_path(db.upcast_mut());
            let file_name = format!("{contract_full_path}.json");

            let mut file = target_dir.open_rw(file_name.clone(), "output file", ws.config())?;
            serde_json::to_writer_pretty(file.deref_mut(), &class)
                .with_context(|| format!("failed to serialize contract: {contract_full_path}"))?;

            let class_hash = compute_class_hash_of_contract_class(&class).with_context(|| {
                format!("problem computing class hash for contract `{contract_full_path}`")
            })?;
            compiled_classes.insert(contract_full_path.into(), (class_hash, class.abi));
        }

        update_manifest(db, ws, &main_crate_ids, compiled_classes, props.build_external_contracts)?;

        Ok(())
    }
}

fn compute_class_hash_of_contract_class(class: &ContractClass) -> Result<FieldElement> {
    let class_str = serde_json::to_string(&class)?;
    let sierra_class = serde_json::from_str::<SierraClass>(&class_str)
        .map_err(|e| anyhow!("error parsing Sierra class: {e}"))?;
    sierra_class.class_hash().map_err(|e| anyhow!("problem hashing sierra contract: {e}"))
}

fn find_project_contracts(
    mut db: &dyn SemanticGroup,
    main_crate_ids: Vec<CrateId>,
    external_contracts: Option<Vec<ContractSelector>>,
) -> Result<Vec<ContractDeclaration>> {
    let internal_contracts = {
        let _ = trace_span!("find_internal_contracts").enter();
        find_contracts(db, &main_crate_ids)
    };

    let external_contracts = if let Some(external_contracts) = external_contracts {
        let _ = trace_span!("find_external_contracts").enter();
        debug!("external contracts selectors: {:?}", external_contracts);

        let crate_ids = external_contracts
            .iter()
            .map(|selector| selector.package().into())
            .unique()
            .map(|package_name: SmolStr| {
                db.upcast_mut().intern_crate(CrateLongId::Real(package_name))
            })
            .collect::<Vec<_>>();
        find_contracts(db, crate_ids.as_ref())
            .into_iter()
            .filter(|decl| {
                external_contracts.iter().any(|selector| {
                    let contract_path = decl.module_id().full_path(db.upcast());
                    contract_path == selector.full_path()
                })
            })
            .collect::<Vec<ContractDeclaration>>()
    } else {
        debug!("no external contracts selected");
        Vec::new()
    };

    Ok(internal_contracts.into_iter().chain(external_contracts).collect())
}

pub fn collect_core_crate_ids(db: &RootDatabase) -> Vec<CrateId> {
    [
        ContractSelector(BASE_CONTRACT_NAME.to_string()),
        ContractSelector(EXECUTOR_CONTRACT_NAME.to_string()),
        ContractSelector(WORLD_CONTRACT_NAME.to_string()),
    ]
    .iter()
    .map(|selector| selector.package().into())
    .unique()
    .map(|package_name: SmolStr| db.intern_crate(CrateLongId::Real(package_name)))
    .collect::<Vec<_>>()
}

pub fn collect_external_crate_ids(
    db: &RootDatabase,
    external_contracts: Vec<ContractSelector>,
) -> Vec<CrateId> {
    external_contracts
        .iter()
        .map(|selector| selector.package().into())
        .unique()
        .map(|package_name: SmolStr| db.intern_crate(CrateLongId::Real(package_name)))
        .collect::<Vec<_>>()
}

fn update_manifest(
    db: &RootDatabase,
    ws: &Workspace<'_>,
    crate_ids: &[CrateId],
    compiled_artifacts: HashMap<SmolStr, (FieldElement, Option<abi::Contract>)>,
    external_contracts: Option<Vec<ContractSelector>>,
) -> anyhow::Result<()> {
    let manifests_dir = ws.manifest_path().parent().unwrap().join(MANIFESTS_DIR).join(BASE_DIR);
    let abi_dir = ws.manifest_path().parent().unwrap().join("abis");

    fn get_compiled_artifact_from_map<'a>(
        artifacts: &'a HashMap<SmolStr, (FieldElement, Option<abi::Contract>)>,
        artifact_name: &str,
    ) -> anyhow::Result<&'a (FieldElement, Option<abi::Contract>)> {
        artifacts.get(artifact_name).context(format!(
            "Contract `{artifact_name}` not found. Did you include `dojo` as a dependency?",
        ))
    }

    let mut crate_ids = crate_ids.to_vec();

    let (hash, abi) = get_compiled_artifact_from_map(&compiled_artifacts, WORLD_CONTRACT_NAME)?;

    write_manifest_and_abi(
        &manifests_dir,
        &abi_dir,
        &mut Manifest::new(
            // abi path will be written by `write_manifest`
            Class { class_hash: *hash, abi: None },
            WORLD_CONTRACT_NAME.into(),
        ),
        abi,
    )?;

    let (hash, abi) = get_compiled_artifact_from_map(&compiled_artifacts, EXECUTOR_CONTRACT_NAME)?;
    write_manifest_and_abi(
        &manifests_dir,
        &abi_dir,
        &mut Manifest::new(Class { class_hash: *hash, abi: None }, EXECUTOR_CONTRACT_NAME.into()),
        abi,
    )?;

    let (hash, abi) = get_compiled_artifact_from_map(&compiled_artifacts, BASE_CONTRACT_NAME)?;
    write_manifest_and_abi(
        &manifests_dir,
        &abi_dir,
        &mut Manifest::new(Class { class_hash: *hash, abi: None }, BASE_CONTRACT_NAME.into()),
        abi,
    )?;

    let mut models = BTreeMap::new();
    let mut contracts = BTreeMap::new();
    let mut computed = BTreeMap::new();

    if let Some(external_contracts) = external_contracts {
        let external_crate_ids = collect_external_crate_ids(db, external_contracts);
        crate_ids.extend(external_crate_ids);
    }

    for crate_id in crate_ids {
        for module_id in db.crate_modules(crate_id).as_ref() {
            let file_infos = db.module_generated_file_infos(*module_id).unwrap_or_default();
            for aux_data in file_infos
                .iter()
                .skip(1)
                .filter_map(|info| info.as_ref().map(|i| &i.aux_data))
                .filter_map(|aux_data| aux_data.as_ref().map(|aux_data| aux_data.0.as_any()))
            {
                if let Some(aux_data) = aux_data.downcast_ref::<StarkNetContractAuxData>() {
                    contracts.extend(get_dojo_contract_artifacts(
                        db,
                        module_id,
                        aux_data,
                        &compiled_artifacts,
                    )?);
                }
                if let Some(aux_data) = aux_data.downcast_ref::<ComputedValuesAuxData>() {
                    get_dojo_computed_values(db, module_id, aux_data, &mut computed);
                }

                if let Some(dojo_aux_data) = aux_data.downcast_ref::<DojoAuxData>() {
                    models.extend(get_dojo_model_artifacts(
                        db,
                        dojo_aux_data,
                        *module_id,
                        &compiled_artifacts,
                    )?);
                }
            }
        }
    }

    computed.into_iter().for_each(|(contract, computed_value_entrypoint)| {
        let contract_data =
            contracts.get_mut(&contract).expect("Error: Computed value contract doesn't exist.");
        contract_data.0.inner.computed = computed_value_entrypoint;
    });

    for model in &models {
        contracts.remove(model.0.as_str());
    }

    for (_, (manifest, abi)) in contracts.iter_mut() {
        write_manifest_and_abi(
            &manifests_dir.join(CONTRACTS_DIR),
            &abi_dir.join(CONTRACTS_DIR),
            manifest,
            abi,
        )?;
    }

    for (_, (manifest, abi)) in models.iter_mut() {
        write_manifest_and_abi(
            &manifests_dir.join(MODELS_DIR),
            &abi_dir.join(MODELS_DIR),
            manifest,
            abi,
        )?;
    }

    Ok(())
}

/// Finds the inline modules annotated as models in the given crate_ids and
/// returns the corresponding Models.
fn get_dojo_model_artifacts(
    db: &RootDatabase,
    aux_data: &DojoAuxData,
    module_id: ModuleId,
    compiled_classes: &HashMap<SmolStr, (FieldElement, Option<abi::Contract>)>,
) -> anyhow::Result<HashMap<String, (Manifest<DojoModel>, Option<abi::Contract>)>> {
    let mut models = HashMap::with_capacity(aux_data.models.len());

    let module_name = module_id.full_path(db);
    let module_name = module_name.as_str();

    for model in &aux_data.models {
        if let Ok(Some(ModuleItemId::Struct(_))) =
            db.module_item_by_name(module_id, model.name.clone().into())
        {
            let model_contract_name = model.name.to_case(Case::Snake);
            let model_full_name = format!("{module_name}::{}", &model_contract_name);

            let compiled_class = compiled_classes.get(model_full_name.as_str()).cloned();

            if let Some((class_hash, abi)) = compiled_class {
                models.insert(
                    model_full_name.clone(),
                    (
                        Manifest::new(
                            dojo_world::manifest::DojoModel {
                                class_hash,
                                abi: None,
                                members: model.members.clone(),
                            },
                            model_full_name.into(),
                        ),
                        abi,
                    ),
                );
            } else {
                println!("Model {} not found in target.", model_full_name.clone());
            }
        }
    }

    Ok(models)
}

fn get_dojo_computed_values(
    db: &RootDatabase,
    module_id: &ModuleId,
    aux_data: &ComputedValuesAuxData,
    computed_values: &mut BTreeMap<SmolStr, Vec<ComputedValueEntrypoint>>,
) {
    if let ModuleId::Submodule(_) = module_id {
        let module_name = module_id.full_path(db);
        let module_name = SmolStr::from(module_name);

        if !computed_values.contains_key(&module_name) {
            computed_values.insert(module_name.clone(), vec![]);
        }
        let computed_vals = computed_values.get_mut(&module_name).unwrap();
        computed_vals.push(ComputedValueEntrypoint {
            contract: module_name,
            entrypoint: aux_data.entrypoint.clone(),
            model: aux_data.model.clone(),
        })
    }
}

fn get_dojo_contract_artifacts(
    db: &RootDatabase,
    module_id: &ModuleId,
    aux_data: &StarkNetContractAuxData,
    compiled_classes: &HashMap<SmolStr, (FieldElement, Option<abi::Contract>)>,
) -> anyhow::Result<HashMap<SmolStr, (Manifest<DojoContract>, Option<abi::Contract>)>> {
    aux_data
        .contracts
        .iter()
        .filter(|name| !matches!(name.as_ref(), "world" | "executor" | "base"))
        .filter(|_name| {
            let module_name = module_id.full_path(db);
            compiled_classes.get(module_name.as_str()).cloned().is_some()
        })
        .map(|name| {
            let module_name = module_id.full_path(db);
            let module_name = module_name.as_str();

            let reads = match SYSTEM_READS.lock().unwrap().get(module_name) {
                Some(models) => {
                    models.clone().into_iter().collect::<BTreeSet<_>>().into_iter().collect()
                }
                None => vec![],
            };

            let write_entries = SYSTEM_WRITES.lock().unwrap();
            let writes = match write_entries.get(module_name) {
                Some(write_ops) => find_module_rw(db, module_id, write_ops),
                None => vec![],
            };

            let (class_hash, abi) = compiled_classes
                .get(module_name)
                .cloned()
                .ok_or(anyhow!("Contract {name} not found in target."))?;

            Ok((
                SmolStr::from(module_name),
                (
                    Manifest::new(
                        DojoContract { writes, reads, class_hash, abi: None, ..Default::default() },
                        module_name.into(),
                    ),
                    abi,
                ),
            ))
        })
        .collect::<anyhow::Result<_>>()
}

fn write_manifest_and_abi<T>(
    manifest_base: &Utf8PathBuf,
    abi_base: &Utf8PathBuf,
    manifest: &mut Manifest<T>,
    abi: &Option<abi::Contract>,
) -> anyhow::Result<()>
where
    T: Serialize + DeserializeOwned + ManifestMethods,
{
    let parts: Vec<&str> = manifest.name.split("::").collect();
    let name: Utf8PathBuf = parts.last().unwrap().into();

    let full_manifest_path = manifest_base.join(name.clone()).with_extension("toml");
    let full_abi_path = abi_base.join(name.clone()).with_extension("json");

    let abi_relative_path = Utf8PathBuf::new().join("abis").join(name).with_extension("json");

    // if file already exists we read it and update `manifest` to make sure immutable properties
    // don't get overriden
    if full_manifest_path.exists() {
        let old_manifest: Manifest<T> =
            toml::from_str(&read_to_string(full_manifest_path.clone())?)?;
        manifest.inner.merge(old_manifest.inner);
    }

    if abi.is_some() {
        manifest.inner.set_abi(Some(abi_relative_path.to_string()));
    }

    let manifest_toml = toml::to_string_pretty(&manifest)?;
    let abi_json = serde_json::to_string_pretty(&abi)?;

    // Create the directory if it doesn't exist
    if let Some(parent) = full_manifest_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }

    // Create the directory if it doesn't exist
    if let Some(parent) = full_abi_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }

    std::fs::write(full_manifest_path.clone(), manifest_toml)
        .expect(&format!("Unable to write manifest file to path: {full_manifest_path}"));
    std::fs::write(full_abi_path.clone(), abi_json)
        .expect(&format!("Unable to write abi file to path: {full_abi_path}"));
    Ok(())
}
