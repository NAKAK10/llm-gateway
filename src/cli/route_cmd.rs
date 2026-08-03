//! `llm-gateway route add|edit` — add or change one route in an existing
//! `config.json`, without regenerating the whole file the way `init` does.
//!
//! Both commands accept the same shape: give a route name plus at least one
//! field to set on the command line, and it's applied directly, headless.
//! Give less than that, and whatever's missing is asked for interactively —
//! `route edit` with no name at all first asks *which* route (a `select`
//! over every route already in the config), then walks through its fields
//! one at a time, each pre-filled with its current value so accepting the
//! default just leaves it unchanged.

use clap::Args;

use crate::config::{Description, ModelConfig, RouteConfig};
use crate::error::{Error, Result};
use crate::paths;

#[derive(Args)]
pub struct AddArgs {
    /// Route name, e.g. `role-writer`. Prompted for if omitted.
    pub name: Option<String>,

    /// Classification text. Repeat for more than one language variant —
    /// see `Description` for why more than one can matter.
    #[arg(long = "description")]
    pub description: Vec<String>,

    /// `<provider>/<model>` for the route's primary target.
    #[arg(long)]
    pub model: Option<String>,

    /// `<provider>/<model>`, repeatable — tried in order if the default
    /// target fails, before the first response byte reaches the client.
    #[arg(long)]
    pub fallback: Vec<String>,
}

#[derive(Args)]
pub struct EditArgs {
    /// Which route to edit. Omit to pick from a list interactively.
    pub name: Option<String>,

    /// Replaces the whole description (every variant) with just this one.
    /// Repeat for more than one language variant.
    #[arg(long = "description")]
    pub description: Vec<String>,

    /// Replaces the default (`model.default`) target.
    #[arg(long)]
    pub model: Option<String>,

    /// Replaces the whole fallback list, in the order given. Repeat to set
    /// more than one.
    #[arg(long)]
    pub fallback: Vec<String>,

    /// Removes every fallback, leaving just the default target. Distinct
    /// from omitting `--fallback`, which is "leave the fallbacks as they
    /// are" — there is no other way to say "I want zero" on the command
    /// line.
    #[arg(long, conflicts_with = "fallback")]
    pub clear_fallbacks: bool,
}

pub fn add(args: AddArgs) -> Result<()> {
    let config_path = paths::config_file();
    let mut config = crate::cli::config_write::read_or_default(&config_path)?;

    let name = match args.name {
        Some(name) => name,
        None => cliclack::input("Route name (e.g. role-writer)").interact()?,
    };

    if config.routes.contains_key(&name) {
        return Err(Error::Other(format!(
            "route `{name}` already exists — use `llm-gateway route edit {name}` instead"
        )));
    }

    let description = if args.description.is_empty() {
        Some(Description(prompt_description_variants(&[])?))
    } else {
        Some(Description(args.description))
    };

    let default = match args.model {
        Some(model) => model,
        None => cliclack::input("Default model (<provider>/<model>)").interact()?,
    };

    let fallbacks = if args.fallback.is_empty() {
        prompt_fallbacks(&[])?
    } else {
        args.fallback
    };

    config.routes.insert(
        name.clone(),
        RouteConfig {
            title: None,
            description,
            model: ModelConfig { default, fallbacks },
        },
    );

    crate::cli::config_write::write_config(&config, &config_path)?;
    cliclack::log::success(format!("added route `{name}` to {}", config_path.display()))?;
    Ok(())
}

pub fn edit(args: EditArgs) -> Result<()> {
    let config_path = paths::config_file();
    let mut config = crate::cli::config_write::read_or_default(&config_path)?;

    if config.routes.is_empty() {
        return Err(Error::Other(
            "no routes configured yet — run `llm-gateway route add` first".to_string(),
        ));
    }

    // Headless: a name plus at least one field to change, applied directly
    // with no prompts at all.
    let any_field_given = !args.description.is_empty()
        || args.model.is_some()
        || !args.fallback.is_empty()
        || args.clear_fallbacks;

    let name = match &args.name {
        Some(name) => name.clone(),
        // No name given at all — the only case that needs a select: with a
        // name, headless or not, there is already exactly one route in play.
        None => {
            let mut select = cliclack::select("Which route?");
            for (route_name, route) in &config.routes {
                let hint = route.model.default.clone();
                select = select.item(route_name.clone(), route_name.clone(), hint);
            }
            select.interact()?
        }
    };

    let Some(route) = config.routes.get(&name).cloned() else {
        return Err(Error::Other(format!(
            "route `{name}` does not exist — run `llm-gateway route add {name}` instead"
        )));
    };

    let mut updated = route.clone();

    if any_field_given {
        if !args.description.is_empty() {
            updated.description = Some(Description(args.description));
        }
        if let Some(model) = args.model {
            updated.model.default = model;
        }
        if args.clear_fallbacks {
            updated.model.fallbacks = Vec::new();
        } else if !args.fallback.is_empty() {
            updated.model.fallbacks = args.fallback;
        }
    } else {
        let current_variants = route
            .description
            .as_ref()
            .map(|d| d.variants().to_vec())
            .unwrap_or_default();
        if cliclack::confirm("Change the description?")
            .initial_value(false)
            .interact()?
        {
            updated.description =
                Some(Description(prompt_description_variants(&current_variants)?));
        }

        if cliclack::confirm(format!(
            "Change the default model? (currently `{}`)",
            route.model.default
        ))
        .initial_value(false)
        .interact()?
        {
            updated.model.default = cliclack::input("Default model (<provider>/<model>)")
                .default_input(&route.model.default)
                .interact()?;
        }

        let fallback_summary = if route.model.fallbacks.is_empty() {
            "none".to_string()
        } else {
            route.model.fallbacks.join(", ")
        };
        if cliclack::confirm(format!(
            "Change the fallbacks? (currently {fallback_summary})"
        ))
        .initial_value(false)
        .interact()?
        {
            updated.model.fallbacks = prompt_fallbacks(&route.model.fallbacks)?;
        }
    }

    config.routes.insert(name.clone(), updated);

    crate::cli::config_write::write_config(&config, &config_path)?;
    cliclack::log::success(format!(
        "updated route `{name}` in {}",
        config_path.display()
    ))?;
    Ok(())
}

/// Loops "one variant, then add another?" until declined — the same shape
/// `init` never had to build (it only ever writes one variant per language
/// picked up front), but a route's classification text can always use a
/// second language added later. `existing` pre-fills the first prompt so
/// re-running this on an already-described route starts from what's there
/// rather than blank.
fn prompt_description_variants(existing: &[String]) -> Result<Vec<String>> {
    let mut variants = Vec::new();
    loop {
        let n = variants.len();
        let mut prompt = cliclack::input(if n == 0 {
            "Description (classification text)".to_string()
        } else {
            format!("Description, variant {} (another language?)", n + 1)
        });
        if let Some(default) = existing.get(n) {
            prompt = prompt.default_input(default);
        }
        variants.push(prompt.interact()?);

        if !cliclack::confirm("Add another language variant?")
            .initial_value(false)
            .interact()?
        {
            break;
        }
    }
    Ok(variants)
}

/// Loops "one fallback, then add another?" the same way
/// [`prompt_description_variants`] does, blank input treated as "done" so a
/// route can end up with zero fallbacks without an extra confirm step.
fn prompt_fallbacks(existing: &[String]) -> Result<Vec<String>> {
    let mut fallbacks = Vec::new();
    loop {
        let n = fallbacks.len();
        let prompt_text = if n == 0 {
            "Fallback target (<provider>/<model>, blank for none)".to_string()
        } else {
            format!("Fallback {} (<provider>/<model>, blank to stop)", n + 1)
        };
        let mut prompt = cliclack::input(prompt_text).required(false);
        if let Some(default) = existing.get(n) {
            prompt = prompt.default_input(default);
        }
        let value: String = prompt.interact()?;
        if value.trim().is_empty() {
            break;
        }
        fallbacks.push(value);
    }
    Ok(fallbacks)
}
