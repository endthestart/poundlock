use super::{ConfigurationError, Store, endpoint_key, now};
use crate::domain::*;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use std::collections::{BTreeSet, HashMap};
use uuid::Uuid;

/// Every column `model_row` reads, in its order.
pub(super) const MODEL_COLUMNS: &str = "id,name,endpoint_id,model_id,context_tokens,max_output_tokens,
    workload_class,wake_policy,node_id,runtime_profile,created_at,
    last_checked_at,last_check_ok,last_check_message,aliases_json,idle_seconds,data_destination,
    model_label,uses_json,thinking,capabilities_json,catalog_json,retirement_json,thinking_budget_tokens,
    destination_from_endpoint";

const DESTINATION_VALUES_MESSAGE: &str =
    "data_destination must be unknown, operator_network or external";
const GATEWAY_DESTINATION_MESSAGE: &str =
    "a gateway declares where prompts go for each route, when the route is published";
const RESTATE_DESTINATION: &str = "this edit points the profile at a different endpoint, model, node or runtime; \
     restate data_destination in the same request";

fn destination_source(inherited: bool) -> &'static str {
    if inherited { "endpoint" } else { "profile" }
}

/// A model server's declaration, or none for a gateway, whose routes each
/// declare their own.
fn server_destination(kind: &str, stored: String) -> Option<String> {
    (kind != "litellm").then_some(stored)
}

/// Declare where a model server sends prompts. Every live model on it
/// inherits the declaration, including one that kept its own from before
/// servers declared it: declared once, it applies to all of them.
fn declare_server(db: &rusqlite::Connection, endpoint_id: &str, destination: &str) -> Result<()> {
    db.execute(
        "UPDATE endpoints SET data_destination=?1 WHERE id=?2 AND removed_at IS NULL",
        params![destination, endpoint_id],
    )?;
    db.execute(
        "UPDATE model_profiles SET data_destination=?1,destination_from_endpoint=1
         WHERE endpoint_id=?2 AND removed_at IS NULL",
        params![destination, endpoint_id],
    )?;
    Ok(())
}

fn resolve_model_uses(
    uses: Option<&Vec<String>>,
    workload_class: Option<&str>,
) -> Result<BTreeSet<String>> {
    crate::domain::resolve_uses(uses, workload_class)
        .map_err(ConfigurationError)?
        .ok_or_else(|| ConfigurationError("uses or workload_class is required").into())
}

impl Store {
    pub fn create_endpoint(&self, input: &CreateEndpoint) -> Result<Endpoint> {
        let id = Uuid::new_v4().to_string();
        let created_at = now();
        let secret = self.secrets.seal(&endpoint_key(&id), &input.api_key)?;
        let managed_identity = serde_json::to_string(&input.managed_identity)?;
        let revision_nonce = Uuid::new_v4().to_string();
        self.connection()?.execute(
            "INSERT INTO endpoints
             (id,name,kind,base_url,api_key_ciphertext,created_at,managed_identity_json,revision_nonce)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                id,
                input.name,
                input.kind,
                input.base_url,
                secret,
                created_at,
                managed_identity,
                revision_nonce
            ],
        )?;
        Ok(Endpoint {
            id,
            name: input.name.clone(),
            kind: input.kind.clone(),
            base_url: input.base_url.clone(),
            api_key_set: !input.api_key.is_empty(),
            created_at,
            managed_identity: input.managed_identity.clone(),
            public_url: None,
            data_destination: server_destination(&input.kind, "unknown".into()),
            last_check: ResourceCheck::default(),
        })
    }

    pub fn list_endpoints(&self) -> Result<Vec<Endpoint>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id,name,kind,base_url,api_key_ciphertext,created_at,
             last_checked_at,last_check_ok,last_check_message,managed_identity_json,public_url,
             data_destination
             FROM endpoints WHERE removed_at IS NULL ORDER BY name",
        )?;
        Ok(statement
            .query_map([], |row| {
                let secret: String = row.get(4)?;
                let kind: String = row.get(2)?;
                let id: String = row.get(0)?;
                Ok(Endpoint {
                    name: row.get(1)?,
                    data_destination: server_destination(&kind, row.get(11)?),
                    kind,
                    base_url: row.get(3)?,
                    api_key_set: !self
                        .secrets
                        .open(&endpoint_key(&id), &secret)
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Text,
                                error.into(),
                            )
                        })?
                        .is_empty(),
                    id,
                    created_at: row.get(5)?,
                    managed_identity: managed_identity_row(row, 9)?,
                    public_url: row.get(10)?,
                    last_check: last_check(row, 6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Set or clear where operators open this service. A link, not a route:
    /// the routing revision does not move.
    pub fn set_endpoint_public_url(&self, id: &str, public_url: Option<&str>) -> Result<bool> {
        Ok(self.connection()?.execute(
            "UPDATE endpoints SET public_url=?1 WHERE id=?2 AND removed_at IS NULL",
            params![public_url, id],
        )? > 0)
    }

    pub fn endpoint_kinds(&self) -> Result<HashMap<String, String>> {
        let connection = self.connection()?;
        let mut statement =
            connection.prepare("SELECT id, kind FROM endpoints WHERE removed_at IS NULL")?;
        Ok(statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<HashMap<String, String>>>()?)
    }

    pub fn routing_revision(&self, profile: &ModelProfile) -> Result<String> {
        let endpoint = self
            .endpoint_secret(&profile.endpoint_id)?
            .ok_or(ConfigurationError("endpoint_id does not exist"))?;
        let (endpoint_nonce, profile_nonce): (String, String) = self.connection()?.query_row(
            "SELECT e.revision_nonce,m.revision_nonce
             FROM model_profiles m JOIN endpoints e ON e.id=m.endpoint_id
             WHERE m.id=?1 AND m.removed_at IS NULL AND e.removed_at IS NULL",
            [&profile.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        // Only where project data goes: labels, limits, uses and names are not a
        // route, so editing them must not force clients to re-approve destinations.
        let value = serde_json::json!({
            "profile": {
                "id": profile.id,
                "model_id": profile.model_id,
                "data_destination": profile.data_destination,
                "node_id": profile.node_id,
                "runtime_profile": profile.runtime_profile,
                "revision_nonce": profile_nonce,
            },
            "endpoint": {
                "id": endpoint.endpoint.id,
                "kind": endpoint.endpoint.kind,
                "base_url": endpoint.endpoint.base_url,
                "credential_hash": crate::crypto::token_hash(&endpoint.api_key),
                "managed_identity": endpoint.endpoint.managed_identity,
                "revision_nonce": endpoint_nonce,
            }
        });
        Ok(crate::crypto::sha256_hex(&serde_json::to_vec(&value)?))
    }

    pub fn endpoint_secret(&self, id: &str) -> Result<Option<EndpointSecret>> {
        let row = self
            .connection()?
            .query_row(
                "SELECT id,name,kind,base_url,api_key_ciphertext,created_at,
                 last_checked_at,last_check_ok,last_check_message,managed_identity_json,public_url,
                 data_destination
                 FROM endpoints WHERE id=?1 AND removed_at IS NULL",
                [id],
                |row| {
                    let kind: String = row.get(2)?;
                    Ok((
                        Endpoint {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            data_destination: server_destination(&kind, row.get(11)?),
                            kind,
                            base_url: row.get(3)?,
                            api_key_set: true,
                            created_at: row.get(5)?,
                            managed_identity: managed_identity_row(row, 9)?,
                            public_url: row.get(10)?,
                            last_check: last_check(row, 6)?,
                        },
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(mut endpoint, ciphertext)| {
            let api_key = self
                .secrets
                .open(&endpoint_key(&endpoint.id), &ciphertext)?;
            endpoint.api_key_set = !api_key.is_empty();
            Ok(EndpointSecret { endpoint, api_key })
        })
        .transpose()
    }

    pub fn create_model_profile(&self, input: &CreateModelProfile) -> Result<ModelProfile> {
        let aliases = normalize_aliases(&input.name, &input.aliases)?;
        self.ensure_profile_names_free(&input.name, &aliases, None)?;
        let uses = resolve_model_uses(input.uses.as_ref(), input.workload_class.as_deref())?;
        let workload_class = primary_workload_class(&uses).to_owned();
        let endpoint = self
            .endpoint_secret(&input.endpoint_id)?
            .ok_or(ConfigurationError("endpoint_id does not exist"))?
            .endpoint;
        let (data_destination, inherited, declaration) =
            self.model_destination(&endpoint, Some(&input.data_destination), None)?;
        let created_at = now();
        let capabilities = stamp_recorded_at(&input.capabilities, &created_at);
        let profile = ModelProfile {
            id: Uuid::new_v4().to_string(),
            name: input.name.clone(),
            aliases,
            endpoint_id: input.endpoint_id.clone(),
            model_id: input.model_id.clone(),
            model_label: input.model_label.clone(),
            context_tokens: input.context_tokens,
            max_output_tokens: input.max_output_tokens,
            uses,
            workload_class,
            wake_policy: input.wake_policy.clone(),
            node_id: input.node_id.clone(),
            runtime_profile: input.runtime_profile.clone(),
            idle_seconds: input.idle_seconds,
            data_destination,
            data_destination_source: destination_source(inherited).into(),
            thinking: input.thinking.clone(),
            thinking_budget_tokens: input.thinking_budget_tokens,
            capabilities,
            catalog: RouteCatalog::default(),
            retirement: Retirement::default(),
            created_at,
            last_check: ResourceCheck::default(),
        };
        self.validate_model_profile(&profile)?;
        let aliases_json = serde_json::to_string(&profile.aliases)?;
        let uses_json = serde_json::to_string(&profile.uses)?;
        let capabilities_json = serde_json::to_string(&profile.capabilities)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        if let Some(destination) = &declaration {
            // The first model on a server nobody declared yet declares it.
            declare_server(&transaction, &profile.endpoint_id, destination)?;
        }
        transaction.execute(
            "INSERT INTO model_profiles
             (id,name,endpoint_id,model_id,context_tokens,max_output_tokens,
              workload_class,wake_policy,node_id,runtime_profile,created_at,aliases_json,idle_seconds,data_destination,model_label,revision_nonce,uses_json,thinking,capabilities_json,catalog_json,retirement_json,thinking_budget_tokens,destination_from_endpoint)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)",
            params![
                profile.id,
                profile.name,
                profile.endpoint_id,
                profile.model_id,
                profile.context_tokens,
                profile.max_output_tokens,
                profile.workload_class,
                profile.wake_policy,
                profile.node_id,
                profile.runtime_profile,
                profile.created_at,
                aliases_json,
                profile.idle_seconds,
                profile.data_destination,
                profile.model_label,
                Uuid::new_v4().to_string(),
                uses_json,
                profile.thinking,
                capabilities_json,
                serde_json::to_string(&profile.catalog)?,
                serde_json::to_string(&profile.retirement)?,
                profile.thinking_budget_tokens,
                inherited
            ],
        )?;
        transaction.commit()?;
        Ok(profile)
    }

    /// Where a model on `endpoint` sends prompts, given what a request asked
    /// for: the destination, whether it is the server's, and a declaration to
    /// record on a server nobody has declared yet.
    ///
    /// A gateway route carries its own declaration. A model server carries one
    /// for every model on it: a model inherits it, and a request may restate
    /// it but not contradict it. Nothing is ever inferred.
    fn model_destination(
        &self,
        endpoint: &Endpoint,
        requested: Option<&str>,
        profile_id: Option<&str>,
    ) -> Result<(String, bool, Option<String>)> {
        if requested.is_some_and(|value| !DATA_DESTINATIONS.contains(&value)) {
            return Err(ConfigurationError(DESTINATION_VALUES_MESSAGE).into());
        }
        let Some(declared) = endpoint.data_destination.as_deref() else {
            return Ok((requested.unwrap_or("unknown").to_owned(), false, None));
        };
        match requested {
            None | Some("unknown") => Ok((declared.to_owned(), true, None)),
            Some(value) if value == declared => Ok((declared.to_owned(), true, None)),
            Some(value) if declared == "unknown" => {
                if self.server_has_other_models(&endpoint.id, profile_id)? {
                    Err(ConfigurationError(
                        "where this server sends prompts is not declared yet; declare it once on \
                         the server, and every model on it inherits it",
                    )
                    .into())
                } else {
                    Ok((value.to_owned(), true, Some(value.to_owned())))
                }
            }
            Some(_) => Err(ConfigurationError(
                "this server already declares where its prompts go; change it on the server, \
                 where the change applies to every model on it",
            )
            .into()),
        }
    }

    fn server_has_other_models(&self, endpoint_id: &str, except: Option<&str>) -> Result<bool> {
        Ok(self.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM model_profiles
             WHERE endpoint_id=?1 AND removed_at IS NULL AND id IS NOT ?2)",
            params![endpoint_id, except],
            |row| row.get(0),
        )?)
    }

    /// Declare where a model server sends prompts, for every model on it.
    /// False when there is no such server.
    pub fn set_endpoint_destination(&self, id: &str, destination: &str) -> Result<bool> {
        if !DATA_DESTINATIONS.contains(&destination) {
            return Err(ConfigurationError(DESTINATION_VALUES_MESSAGE).into());
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let kind: Option<String> = transaction
            .query_row(
                "SELECT kind FROM endpoints WHERE id=?1 AND removed_at IS NULL",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(kind) = kind else {
            return Ok(false);
        };
        if kind == "litellm" {
            return Err(ConfigurationError(GATEWAY_DESTINATION_MESSAGE).into());
        }
        declare_server(&transaction, id, destination)?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn list_model_profiles(&self) -> Result<Vec<ModelProfile>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(&format!(
            "SELECT {MODEL_COLUMNS} FROM model_profiles WHERE removed_at IS NULL ORDER BY name"
        ))?;
        Ok(statement
            .query_map([], model_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn model_profile(&self, id: &str) -> Result<Option<ModelProfile>> {
        Ok(self
            .connection()?
            .query_row(
                &format!(
                    "SELECT {MODEL_COLUMNS} FROM model_profiles WHERE id=?1 AND removed_at IS NULL"
                ),
                [id],
                model_row,
            )
            .optional()?)
    }

    pub fn model_profile_by_name(&self, name: &str) -> Result<Option<ModelProfile>> {
        if let Some(profile) = self
            .connection()?
            .query_row(
                &format!("SELECT {MODEL_COLUMNS} FROM model_profiles WHERE name=?1 AND removed_at IS NULL"),
                [name],
                model_row,
            )
            .optional()?
        {
            return Ok(Some(profile));
        }
        Ok(self
            .list_model_profiles()?
            .into_iter()
            .find(|profile| profile.aliases.iter().any(|alias| alias == name)))
    }

    pub fn patch_model_profile(
        &self,
        id: &str,
        input: &crate::domain::PatchModelProfile,
    ) -> Result<Option<ModelProfile>> {
        let Some(mut profile) = self.model_profile(id)? else {
            return Ok(None);
        };
        if let Some(name) = &input.name {
            profile.name = name.trim().to_owned();
        }
        if let Some(aliases) = &input.aliases {
            profile.aliases = normalize_aliases(&profile.name, aliases)?;
        }
        profile.aliases = normalize_aliases(&profile.name, &profile.aliases)?;
        self.ensure_profile_names_free(&profile.name, &profile.aliases, Some(id))?;
        let previous_destination = profile.data_destination.clone();
        let previous_source = profile.data_destination_source.clone();
        let previous_route = (
            profile.endpoint_id.clone(),
            profile.model_id.clone(),
            profile.node_id.clone(),
            profile.runtime_profile.clone(),
        );
        if let Some(endpoint_id) = &input.endpoint_id {
            profile.endpoint_id = endpoint_id.clone();
        }
        if let Some(model_id) = &input.model_id {
            profile.model_id = model_id.trim().to_owned();
        }
        if let Some(context_tokens) = input.context_tokens {
            profile.context_tokens = context_tokens;
        }
        if let Some(max_output_tokens) = input.max_output_tokens {
            profile.max_output_tokens = max_output_tokens;
        }
        if let Some(uses) =
            crate::domain::resolve_uses(input.uses.as_ref(), input.workload_class.as_deref())
                .map_err(ConfigurationError)?
        {
            profile.uses = uses;
            profile.workload_class = primary_workload_class(&profile.uses).to_owned();
        }
        if let Some(wake_policy) = &input.wake_policy {
            profile.wake_policy = wake_policy.clone();
        }
        match (&input.node_id, &input.runtime_profile) {
            (None, None) => {}
            (Some(node_id), Some(runtime_profile)) => {
                if node_id.is_empty() && runtime_profile.is_empty() {
                    profile.node_id = None;
                    profile.runtime_profile = None;
                } else if node_id.is_empty() || runtime_profile.is_empty() {
                    return Err(ConfigurationError(
                        "node_id and runtime_profile must be configured together",
                    )
                    .into());
                } else {
                    profile.node_id = Some(node_id.clone());
                    profile.runtime_profile = Some(runtime_profile.clone());
                }
            }
            _ => {
                return Err(ConfigurationError(
                    "node_id and runtime_profile must be configured together",
                )
                .into());
            }
        }
        let route_changed = previous_route
            != (
                profile.endpoint_id.clone(),
                profile.model_id.clone(),
                profile.node_id.clone(),
                profile.runtime_profile.clone(),
            );
        if let Some(label) = &input.model_label {
            profile.model_label = label.clone();
        } else if route_changed {
            profile.model_label = None;
        }
        let endpoint = self
            .endpoint_secret(&profile.endpoint_id)?
            .ok_or(ConfigurationError("endpoint_id does not exist"))?
            .endpoint;
        let requested = input.data_destination.as_deref();
        let mut declaration = None;
        if endpoint.data_destination.is_some() {
            // A model server declares where its prompts go, and its models
            // inherit it; moving within it keeps it. Restating a model's own
            // value is always accepted, so a model that kept its own from
            // before servers declared it can be edited as before.
            let restating = !route_changed && requested == Some(previous_destination.as_str());
            if previous_source == "endpoint" || route_changed || (requested.is_some() && !restating)
            {
                let (destination, _, declare) =
                    self.model_destination(&endpoint, requested, Some(id))?;
                if requested.is_none()
                    && previous_destination != "unknown"
                    && destination != previous_destination
                {
                    return Err(ConfigurationError(RESTATE_DESTINATION).into());
                }
                profile.data_destination = destination;
                profile.data_destination_source = "endpoint".into();
                declaration = declare;
            }
        } else if let Some(destination) = requested {
            profile.data_destination = destination.to_owned();
            profile.data_destination_source = "profile".into();
        } else if route_changed {
            // A re-pointed route may send data somewhere else, so the old
            // declaration cannot carry over. Resetting it to `unknown` used to
            // happen silently: the edit returned 200, and the profile then
            // refused every request with a 403 until someone noticed. An
            // operator declares the destination in the same edit instead.
            if previous_destination != "unknown" {
                return Err(ConfigurationError(RESTATE_DESTINATION).into());
            }
            profile.data_destination = "unknown".into();
            profile.data_destination_source = "profile".into();
        }
        if let Some(seconds) = input.idle_seconds {
            profile.idle_seconds = seconds;
        }
        if let Some(thinking) = &input.thinking {
            profile.thinking = thinking.clone();
        }
        if let Some(budget) = input.thinking_budget_tokens {
            profile.thinking_budget_tokens = budget;
        }
        if let Some(capabilities) = &input.capabilities {
            // A paste of the Toolkit's block replaces the whole list; it is
            // not a per-entry edit.
            profile.capabilities = stamp_recorded_at(capabilities, &now());
        }
        if route_changed && input.capabilities.is_none() {
            // Evidence describes the model that was measured, not the profile
            // name pointing at it. A new endpoint, model, node or runtime
            // profile is a different thing under the same name, so the old
            // scores say nothing about it — the same reasoning that clears
            // the label and resets the destination just above. A paste that
            // re-points and re-measures in one patch keeps what it supplied.
            profile.capabilities.clear();
        }
        if input.context_tokens.is_some() || input.thinking.is_some() {
            // A stored measurement describes a specific context window and
            // reasoning default. Once either changes here, entries that no
            // longer match describe a configuration that no longer exists —
            // the same reasoning as the routing-revision rotation above,
            // applied to evidence instead of routing.
            let context_tokens = profile.context_tokens;
            let thinking = profile.thinking.clone();
            profile.capabilities.retain(|capability| {
                crate::domain::capability_settings_match(
                    context_tokens,
                    &thinking,
                    &capability.measured_settings,
                )
            });
        }
        let previous_catalog = profile.catalog.clone();
        let previous_context = profile.context_tokens;
        if let Some(catalog) = &input.catalog {
            profile.catalog = catalog.clone();
        } else if route_changed {
            // A catalog entry describes what served the old route: its model,
            // its price and the levels it was shown to accept. None of that is
            // known about the new one.
            profile.catalog = RouteCatalog::default();
        }
        self.validate_model_profile(&profile)?;
        let aliases_json = serde_json::to_string(&profile.aliases)?;
        let uses_json = serde_json::to_string(&profile.uses)?;
        let capabilities_json = serde_json::to_string(&profile.capabilities)?;
        let catalog_json = serde_json::to_string(&profile.catalog)?;
        // A client that approved this route approved what it serves, where the
        // data goes and how much it can read. A change to any of them is a
        // different route under the same name, so the revision moves and the
        // client is asked again. A price refresh is not: spend is bounded by the
        // gateway's budgets, and re-asking for consent on every price update
        // would train users to approve without reading.
        let rotate_nonce = route_changed
            || profile.data_destination != previous_destination
            || profile.context_tokens != previous_context
            || profile.catalog.served_model != previous_catalog.served_model;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        if let Some(destination) = &declaration {
            declare_server(&transaction, &profile.endpoint_id, destination)?;
        }
        transaction.execute(
            "UPDATE model_profiles SET aliases_json=?1, endpoint_id=?2, model_id=?3,
             context_tokens=?4, max_output_tokens=?5, workload_class=?6, wake_policy=?7,
             node_id=?8, runtime_profile=?9, idle_seconds=?11, name=?12, data_destination=?13, model_label=?14,
             last_checked_at=NULL,last_check_ok=NULL,last_check_message=NULL,
             revision_nonce=CASE WHEN ?18 THEN ?15 ELSE revision_nonce END,uses_json=?16,thinking=?17,
             capabilities_json=?19,catalog_json=?20,thinking_budget_tokens=?21,destination_from_endpoint=?22
             WHERE id=?10 AND removed_at IS NULL",
            params![
                aliases_json,
                profile.endpoint_id,
                profile.model_id,
                profile.context_tokens,
                profile.max_output_tokens,
                profile.workload_class,
                profile.wake_policy,
                profile.node_id,
                profile.runtime_profile,
                id,
                profile.idle_seconds,
                profile.name,
                profile.data_destination,
                profile.model_label,
                Uuid::new_v4().to_string(),
                uses_json,
                profile.thinking,
                rotate_nonce,
                capabilities_json,
                catalog_json,
                profile.thinking_budget_tokens,
                profile.data_destination_source == "endpoint"
            ],
        )?;
        transaction.commit()?;
        profile.last_check = ResourceCheck::default();
        Ok(Some(profile))
    }

    fn ensure_profile_names_free(
        &self,
        name: &str,
        aliases: &[String],
        except_id: Option<&str>,
    ) -> Result<()> {
        for profile in self.list_model_profiles()? {
            if except_id.is_some_and(|id| id == profile.id) {
                continue;
            }
            if profile.name == name || profile.aliases.iter().any(|alias| alias == name) {
                return Err(ConfigurationError("a resource with that name already exists").into());
            }
            for alias in aliases {
                if profile.name == *alias
                    || profile.aliases.iter().any(|existing| existing == alias)
                {
                    return Err(
                        ConfigurationError("a resource with that name already exists").into(),
                    );
                }
            }
        }
        Ok(())
    }

    pub fn list_harness_profiles(&self) -> Result<Vec<HarnessProfile>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id,name,kind,version,created_at FROM harness_profiles ORDER BY name",
        )?;
        Ok(statement
            .query_map([], |row| {
                Ok(HarnessProfile {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    kind: row.get(2)?,
                    version: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn harness_profile(&self, id: &str) -> Result<Option<HarnessProfile>> {
        Ok(self
            .connection()?
            .query_row(
                "SELECT id,name,kind,version,created_at FROM harness_profiles WHERE id=?1",
                [id],
                |row| {
                    Ok(HarnessProfile {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        kind: row.get(2)?,
                        version: row.get(3)?,
                        created_at: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn create_project(&self, input: &CreateProject) -> Result<Project> {
        validate_project(&input.name, &input.roots)?;
        let instruction_files = normalize_instruction_files(&input.instruction_files)?;
        let project = Project {
            id: Uuid::new_v4().to_string(),
            name: input.name.clone(),
            roots: input.roots.clone(),
            instruction_files,
            created_at: now(),
        };
        let roots = serde_json::to_string(&project.roots)?;
        let instruction_files = serde_json::to_string(&project.instruction_files)?;
        self.connection()?.execute(
            "INSERT INTO projects (id,name,roots_json,created_at,instruction_files_json)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                project.id,
                project.name,
                roots,
                project.created_at,
                instruction_files
            ],
        )?;
        Ok(project)
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT id,name,roots_json,created_at,instruction_files_json FROM projects WHERE removed_at IS NULL ORDER BY name")?;
        Ok(statement
            .query_map([], project_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn project(&self, id: &str) -> Result<Option<Project>> {
        Ok(self
            .connection()?
            .query_row(
                "SELECT id,name,roots_json,created_at,instruction_files_json FROM projects WHERE id=?1 AND removed_at IS NULL",
                [id],
                project_row,
            )
            .optional()?)
    }

    pub fn patch_project(
        &self,
        id: &str,
        input: &crate::domain::PatchProject,
    ) -> Result<Option<Project>> {
        let Some(mut project) = self.project(id)? else {
            return Ok(None);
        };
        if let Some(name) = &input.name {
            project.name = name.trim().into();
        }
        if let Some(roots) = &input.roots {
            project.roots = roots.clone();
        }
        if let Some(files) = &input.instruction_files {
            project.instruction_files = normalize_instruction_files(files)?;
        }
        validate_project(&project.name, &project.roots)?;
        self.connection()?.execute(
            "UPDATE projects SET name=?1, roots_json=?2, instruction_files_json=?3
             WHERE id=?4 AND removed_at IS NULL",
            params![
                project.name,
                serde_json::to_string(&project.roots)?,
                serde_json::to_string(&project.instruction_files)?,
                id
            ],
        )?;
        Ok(Some(project))
    }

    pub fn update_endpoint(&self, id: &str, input: &CreateEndpoint) -> Result<Option<Endpoint>> {
        self.update_endpoint_with_destination(id, input, None)
    }

    /// Re-save an endpoint and, for a model server, optionally declare where
    /// it sends prompts in the same transaction.
    pub fn update_endpoint_with_destination(
        &self,
        id: &str,
        input: &CreateEndpoint,
        destination: Option<&str>,
    ) -> Result<Option<Endpoint>> {
        if destination.is_some_and(|value| !DATA_DESTINATIONS.contains(&value)) {
            return Err(ConfigurationError(DESTINATION_VALUES_MESSAGE).into());
        }
        let gateway = input.kind == "litellm";
        if gateway && destination.is_some_and(|value| value != "unknown") {
            return Err(ConfigurationError(GATEWAY_DESTINATION_MESSAGE).into());
        }
        let Some(previous) = self.endpoint_secret(id)? else {
            return Ok(None);
        };
        // Where the endpoint sends data is its kind and address. Rotating its
        // key changes neither, and treating a rotation as a re-point reset the
        // destination of every profile on the endpoint, taking them offline.
        let route_changed =
            previous.endpoint.kind != input.kind || previous.endpoint.base_url != input.base_url;
        let secret = self.secrets.seal(&endpoint_key(id), &input.api_key)?;
        let managed_identity = serde_json::to_string(&input.managed_identity)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let updated = transaction.execute(
            "UPDATE endpoints SET name=?1,kind=?2,base_url=?3,api_key_ciphertext=?4,
             managed_identity_json=?5,revision_nonce=?6,
             last_checked_at=NULL,last_check_ok=NULL,last_check_message=NULL
             WHERE id=?7 AND removed_at IS NULL",
            params![
                input.name,
                input.kind,
                input.base_url,
                secret,
                managed_identity,
                Uuid::new_v4().to_string(),
                id
            ],
        )?;
        if updated == 0 {
            return Ok(None);
        }
        if route_changed {
            // Repointing an endpoint moves the route of every profile on it,
            // so where their data goes is unknown until declared again, and
            // their measurements describe models it may no longer serve. Same
            // rule as `patch_model_profile`. A model server's models go on
            // inheriting its declaration; a gateway's routes each need one.
            transaction.execute(
                "UPDATE endpoints SET data_destination='unknown' WHERE id=?1",
                [id],
            )?;
            transaction.execute(
                "UPDATE model_profiles SET data_destination='unknown',destination_from_endpoint=?2,
                 model_label=NULL,capabilities_json='[]' WHERE endpoint_id=?1 AND removed_at IS NULL",
                params![id, !gateway],
            )?;
        }
        if let Some(destination) = destination.filter(|_| !gateway) {
            declare_server(&transaction, id, destination)?;
        }
        transaction.commit()?;
        drop(connection);
        Ok(self
            .list_endpoints()?
            .into_iter()
            .find(|item| item.id == id))
    }

    pub fn remove_model_profile(&self, id: &str) -> Result<bool> {
        // Keep the row for historical foreign keys; accepted jobs use their frozen contract.
        Ok(self.connection()?.execute(
            "UPDATE model_profiles SET removed_at=?1,name='removed-'||id,aliases_json='[]'
             WHERE id=?2 AND removed_at IS NULL",
            params![now(), id],
        )? > 0)
    }

    pub fn remove_endpoint(&self, id: &str) -> Result<bool> {
        let connection = self.connection()?;
        let used: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM model_profiles WHERE endpoint_id=?1 AND removed_at IS NULL)",
            [id], |row| row.get(0))?;
        if used {
            return Err(
                ConfigurationError("Remove or move this endpoint's model profiles first").into(),
            );
        }
        let active_mappings: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM managed_litellm_keys
             WHERE endpoint_id=?1 AND status NOT IN ('revoked','deleted'))",
            [id],
            |row| row.get(0),
        )?;
        if active_mappings {
            return Err(ConfigurationError(
                "Revoke managed client keys before removing this endpoint",
            )
            .into());
        }
        Ok(connection.execute(
            "UPDATE endpoints SET removed_at=?1,name='removed-'||id,api_key_ciphertext=''
             WHERE id=?2 AND removed_at IS NULL",
            params![now(), id],
        )? > 0)
    }

    pub fn remove_project(&self, id: &str) -> Result<bool> {
        Ok(self.connection()?.execute(
            "UPDATE projects SET removed_at=?1,name='removed-'||id WHERE id=?2 AND removed_at IS NULL",
            params![now(),id])? > 0)
    }

    fn validate_model_profile(&self, profile: &ModelProfile) -> Result<()> {
        let error = if !DATA_DESTINATIONS.contains(&profile.data_destination.as_str()) {
            Some(DESTINATION_VALUES_MESSAGE)
        } else if !MODEL_PROFILE_THINKING_VALUES.contains(&profile.thinking.as_str()) {
            Some("thinking must be model_default or off")
        } else if let Some(message) = validate_thinking_budget(profile) {
            Some(message)
        } else if profile.name.trim().is_empty() || profile.model_id.trim().is_empty() {
            Some("Profile name and model ID are required")
        } else if profile.model_label.as_ref().is_some_and(|s| {
            s.trim().is_empty() || s.len() > 256 || s.chars().any(char::is_control)
        }) {
            Some("Model label must be 1 to 256 bytes without control characters")
        } else if profile.context_tokens < 1024
            || profile.max_output_tokens == 0
            || profile.max_output_tokens > profile.context_tokens
        {
            Some("token limits are inconsistent")
        } else if profile.idle_seconds.is_some_and(|s| s > 3600) {
            Some("Idle delay must be at most 60 minutes")
        } else if !["never", "on-demand", "manual"].contains(&profile.wake_policy.as_str()) {
            Some("unknown wake_policy")
        } else if let Some(message) = validate_capabilities(&profile.capabilities) {
            Some(message)
        } else if let Some(message) = validate_catalog(&profile.catalog) {
            Some(message)
        } else if profile.node_id.is_some() != profile.runtime_profile.is_some()
            || profile
                .runtime_profile
                .as_ref()
                .is_some_and(|s| s.trim().is_empty())
        {
            Some("node_id and runtime_profile must be configured together")
        } else if self.endpoint_secret(&profile.endpoint_id)?.is_none() {
            Some("endpoint_id does not exist")
        } else {
            None
        };
        if let Some(message) = error {
            return Err(ConfigurationError(message).into());
        }
        if let Some(id) = &profile.node_id {
            let node = self.node(id, 15)?.filter(|node| node.status != "revoked");
            if node.is_none() {
                return Err(ConfigurationError("Select an enrolled machine").into());
            }
        }
        Ok(())
    }
}

/// A measurement's `recorded_at` is when Poundlock stored it, never a
/// client-supplied value; stamp every entry with the same timestamp used for
/// the surrounding write.
fn stamp_recorded_at(
    capabilities: &[CapabilityMeasurement],
    recorded_at: &str,
) -> Vec<CapabilityMeasurement> {
    capabilities
        .iter()
        .cloned()
        .map(|mut capability| {
            capability.recorded_at = recorded_at.to_owned();
            capability
        })
        .collect()
}

pub(crate) fn validate_capabilities(
    capabilities: &[CapabilityMeasurement],
) -> Option<&'static str> {
    let mut seen = std::collections::HashSet::new();
    for capability in capabilities {
        if !MODEL_PROFILE_CAPABILITY_NAMES.contains(&capability.name.as_str()) {
            return Some("unknown capability name");
        }
        if !seen.insert(capability.name.as_str()) {
            return Some("at most one capability measurement is allowed per name");
        }
        if !MODEL_PROFILE_CAPABILITY_STATUSES.contains(&capability.status.as_str()) {
            return Some("unknown capability status");
        }
        if capability.passed > capability.attempted {
            return Some("a capability cannot pass more attempts than it made");
        }
        if chrono::DateTime::parse_from_rfc3339(&capability.measured_at).is_err() {
            return Some("capability measured_at must be an RFC 3339 timestamp");
        }
        if capability.source.trim().is_empty() {
            return Some("capability source is required");
        }
        if capability.source == CAPABILITY_SOURCE_TOOLKIT
            && (capability.run_ids.is_empty() || capability.result_sha256.trim().is_empty())
        {
            return Some("a poundlock_toolkit capability requires run_ids and result_sha256");
        }
        if !MODEL_PROFILE_THINKING_VALUES.contains(&capability.measured_settings.reasoning.as_str())
        {
            return Some("capability measured_settings.reasoning must be model_default or off");
        }
        if capability.measured_settings.context_tokens == 0 {
            return Some("capability measured_settings.context_tokens must be positive");
        }
        if capability.note.len() > CAPABILITY_NOTE_MAX_BYTES
            || capability.note.chars().any(char::is_control)
        {
            return Some("capability note is too long or contains control characters");
        }
    }
    None
}

pub(super) fn managed_identity_row(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<ManagedIdentityPolicy> {
    let raw: Option<String> = row.get(index)?;
    serde_json::from_str(raw.as_deref().unwrap_or("{}")).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, error.into())
    })
}

pub(super) fn model_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ModelProfile> {
    let aliases = parse_json_vec(row.get::<_, Option<String>>(14)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(14, rusqlite::types::Type::Text, error.into())
    })?;
    let uses: BTreeSet<String> =
        serde_json::from_str(&row.get::<_, String>(18)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(18, rusqlite::types::Type::Text, error.into())
        })?;
    // workload_class is always derived from `uses`, never trusted from the
    // column directly, so the two can never drift apart.
    let workload_class = primary_workload_class(&uses).to_owned();
    let capabilities = parse_capabilities(row.get::<_, Option<String>>(20)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(20, rusqlite::types::Type::Text, error.into())
    })?;
    Ok(ModelProfile {
        id: row.get(0)?,
        name: row.get(1)?,
        aliases,
        endpoint_id: row.get(2)?,
        model_id: row.get(3)?,
        context_tokens: row.get(4)?,
        max_output_tokens: row.get(5)?,
        uses,
        workload_class,
        wake_policy: row.get(7)?,
        node_id: row.get(8)?,
        runtime_profile: row.get(9)?,
        created_at: row.get(10)?,
        last_check: last_check(row, 11)?,
        idle_seconds: row.get(15)?,
        data_destination: row.get(16)?,
        data_destination_source: destination_source(row.get::<_, i64>(24)? != 0).into(),
        model_label: row.get(17)?,
        thinking: row.get(19)?,
        thinking_budget_tokens: row.get(23)?,
        capabilities,
        catalog: json_column(row, 21)?,
        retirement: json_column(row, 22)?,
    })
}

/// A default thinking budget is a llama.cpp request field: elsewhere it would
/// be dropped without a word, and one above the output limit bounds nothing.
fn validate_thinking_budget(profile: &ModelProfile) -> Option<&'static str> {
    let budget = profile.thinking_budget_tokens?;
    if !crate::domain::serves_llamacpp(profile) {
        Some("thinking_budget_tokens applies only to profiles on the llamacpp runtime")
    } else if budget == 0 || budget > profile.max_output_tokens {
        Some("thinking_budget_tokens must be between 1 and max_output_tokens")
    } else {
        None
    }
}

/// Refuse a catalog entry that would publish something Poundlock cannot stand
/// behind: a reasoning value outside the vocabulary it relays, a price with no
/// stated source, or display text a client might render.
fn validate_catalog(catalog: &RouteCatalog) -> Option<&'static str> {
    let mut seen = BTreeSet::new();
    for level in &catalog.reasoning_levels {
        if !REASONING_LEVELS.contains(&level.as_str()) {
            return Some("reasoning levels must be none, minimal, low, medium, high or xhigh");
        }
        if !seen.insert(level) {
            return Some("reasoning levels must not repeat");
        }
    }
    if !catalog.reasoning_levels.is_empty() && catalog.reasoning_verified_at.is_none() {
        return Some("reasoning levels need the time they were verified");
    }
    let text_ok = |value: &Option<String>| {
        value.as_ref().is_none_or(|text| {
            !text.trim().is_empty() && text.len() <= 256 && !text.chars().any(char::is_control)
        })
    };
    if !text_ok(&catalog.served_model) || !text_ok(&catalog.provider) {
        return Some("served model and provider must be 1 to 256 bytes without control characters");
    }
    if let Some(pricing) = &catalog.pricing {
        let price_ok = |value: f64| (0.0..1_000_000.0).contains(&value);
        if !price_ok(pricing.input_per_mtok) || !price_ok(pricing.output_per_mtok) {
            return Some("prices must be finite, non-negative amounts per million tokens");
        }
        if !PRICING_SOURCES.contains(&pricing.source.as_str()) {
            return Some(
                "pricing source must be litellm_route, litellm_cost_map, openrouter_catalog, local or operator",
            );
        }
    }
    None
}

/// A JSON object column that defaults to empty. A row written before the
/// column existed reads as `{}`, which every such type accepts.
fn json_column<T: serde::de::DeserializeOwned + Default>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<T> {
    let raw: Option<String> = row.get(index)?;
    match raw.as_deref().map(str::trim) {
        None | Some("") => Ok(T::default()),
        Some(text) => serde_json::from_str(text).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                error.into(),
            )
        }),
    }
}

fn last_check(row: &rusqlite::Row<'_>, start: usize) -> rusqlite::Result<ResourceCheck> {
    let ok: Option<i64> = row.get(start + 1)?;
    Ok(ResourceCheck {
        last_checked_at: row.get(start)?,
        last_check_ok: ok.map(|value| value != 0),
        last_check_message: row.get(start + 2)?,
    })
}

pub(super) fn project_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    let roots: String = row.get(2)?;
    let roots = serde_json::from_str(&roots).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, error.into())
    })?;
    let instruction_files = parse_json_vec(row.get::<_, Option<String>>(4)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, error.into())
    })?;
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        roots,
        instruction_files,
        created_at: row.get(3)?,
    })
}

fn parse_json_vec(raw: Option<String>) -> anyhow::Result<Vec<String>> {
    match raw.as_deref() {
        None | Some("") => Ok(Vec::new()),
        Some(value) => Ok(serde_json::from_str(value)?),
    }
}

fn parse_capabilities(raw: Option<String>) -> anyhow::Result<Vec<CapabilityMeasurement>> {
    match raw.as_deref() {
        None | Some("") => Ok(Vec::new()),
        Some(value) => Ok(serde_json::from_str(value)?),
    }
}

fn normalize_aliases(name: &str, aliases: &[String]) -> Result<Vec<String>> {
    let mut unique = Vec::new();
    for alias in aliases {
        let alias = alias.trim();
        if alias.is_empty() {
            continue;
        }
        if alias == name {
            return Err(ConfigurationError("aliases must not repeat the profile name").into());
        }
        if unique.iter().any(|existing: &String| existing == alias) {
            return Err(ConfigurationError("aliases must be unique").into());
        }
        unique.push(alias.to_owned());
    }
    Ok(unique)
}

fn normalize_instruction_files(files: &[String]) -> Result<Vec<String>> {
    let mut unique = Vec::new();
    for file in files {
        let file = file.trim();
        if file.is_empty() {
            continue;
        }
        let path = std::path::Path::new(file);
        if path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(ConfigurationError(
                "instruction files must be relative paths without '..'",
            )
            .into());
        }
        if unique.iter().any(|existing: &String| existing == file) {
            return Err(ConfigurationError("instruction files must be unique").into());
        }
        unique.push(file.to_owned());
    }
    Ok(unique)
}

fn validate_project(name: &str, roots: &[String]) -> Result<()> {
    if name.trim().is_empty() || roots.is_empty() {
        return Err(ConfigurationError(
            "Project name and at least one repository path are required",
        )
        .into());
    }
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        let path = std::path::Path::new(root);
        if root.trim().is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(ConfigurationError(
                "Repository paths must be relative and cannot contain '..'",
            )
            .into());
        }
        if !seen.insert(root) {
            return Err(ConfigurationError("Repository paths must be unique").into());
        }
    }
    Ok(())
}
