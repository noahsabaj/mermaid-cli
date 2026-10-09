//! What a session's tokens cost in money, for `/usage`.
//!
//! The reducer never knows a price on its own: `/usage` asks the effect layer
//! (`Cmd::ResolveModelPrices`) for each model in `Session::usage_by_model`,
//! and the answer comes back as data (`Msg::ModelPricesResolved`). A price is
//! a fact from the user's config or a public catalog, never a table compiled
//! in here, so a vendor's price change needs no Mermaid release.

use std::collections::BTreeMap;

use crate::state::TokenUsageTotals;

/// List prices in US dollars per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    /// Cache reads. `None` charges them at the `input` rate.
    #[serde(default)]
    pub cache_read: Option<f64>,
    /// Cache writes. `None` charges them at the `input` rate.
    #[serde(default)]
    pub cache_write: Option<f64>,
}

impl ModelPrice {
    /// Dollars for `usage` at these prices. Token components are disjoint
    /// (`TokenUsageTotals`), so each is charged once; reasoning tokens are
    /// output tokens.
    #[must_use]
    pub fn cost_usd(&self, usage: &TokenUsageTotals) -> f64 {
        // A session never nears u32::MAX tokens per model; saturate rather
        // than cast so no precision-losing conversion is needed.
        let per_million = |tokens: usize, rate: f64| {
            f64::from(u32::try_from(tokens).unwrap_or(u32::MAX)) * rate / 1_000_000.0
        };
        per_million(usage.prompt_tokens, self.input)
            + per_million(
                usage.cached_input_tokens,
                self.cache_read.unwrap_or(self.input),
            )
            + per_million(
                usage.cache_creation_input_tokens,
                self.cache_write.unwrap_or(self.input),
            )
            + per_million(usage.output_total_tokens(), self.output)
    }
}

/// Where a price came from, shown beside it so the user can judge it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceSource {
    /// `[pricing."<model>"]` in the user's config.
    Config,
    /// The public model catalog (`[pricing] catalog_url`).
    Catalog,
}

/// The answer for one model.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PriceLookup {
    Priced {
        price: ModelPrice,
        source: PriceSource,
    },
    /// Runs on this machine: no per-token charge.
    Local,
    /// No price in config or the catalog.
    Unknown,
}

/// Lookups keyed by model id, as `Msg::ModelPricesResolved` carries them.
pub type ModelPrices = BTreeMap<String, PriceLookup>;

/// The `/usage` cost block: one line per model, then the total of the ones
/// with a known price.
#[must_use]
pub fn cost_lines(usage: &crate::UsageByModel, prices: &ModelPrices) -> Vec<String> {
    let mut lines = vec!["Cost (estimated at list prices):".to_string()];
    let mut total = 0.0;
    let mut unpriced = 0usize;
    for (model, totals) in usage {
        let line = match prices.get(model).copied().unwrap_or(PriceLookup::Unknown) {
            PriceLookup::Priced { price, source } => {
                let cost = price.cost_usd(totals);
                total += cost;
                let from = match source {
                    PriceSource::Config => "config",
                    PriceSource::Catalog => "catalog",
                };
                format!("  {model}: {} ({from})", format_usd(cost))
            },
            PriceLookup::Local => format!("  {model}: $0 (local)"),
            PriceLookup::Unknown => {
                unpriced += 1;
                format!("  {model}: unknown price (set [pricing.\"{model}\"] in config)")
            },
        };
        lines.push(line);
    }
    let total = format_usd(total);
    lines.push(match unpriced {
        0 => format!("  Total: {total}"),
        1 => format!("  Total: {total}, without 1 model with no price"),
        n => format!("  Total: {total}, without {n} models with no price"),
    });
    lines
}

/// Dollars to the cent, or to four places below a cent so a short session
/// does not read as free.
#[must_use]
pub fn format_usd(amount: f64) -> String {
    if amount > 0.0 && amount < 0.01 {
        format!("${amount:.4}")
    } else {
        format!("${amount:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn totals(prompt: usize, cached: usize, written: usize, out: usize) -> TokenUsageTotals {
        TokenUsageTotals {
            prompt_tokens: prompt,
            completion_tokens: out,
            cached_input_tokens: cached,
            cache_creation_input_tokens: written,
            reasoning_output_tokens: 0,
        }
    }

    const PRICE: ModelPrice = ModelPrice {
        input: 3.0,
        output: 15.0,
        cache_read: Some(0.3),
        cache_write: Some(3.75),
    };

    #[test]
    fn each_component_is_charged_at_its_own_rate() {
        let usage = totals(1_000_000, 1_000_000, 1_000_000, 1_000_000);
        let cost = PRICE.cost_usd(&usage);
        assert!((cost - (3.0 + 0.3 + 3.75 + 15.0)).abs() < 1e-9);
    }

    #[test]
    fn reasoning_tokens_are_output_and_missing_cache_rates_fall_back_to_input() {
        let usage = TokenUsageTotals {
            reasoning_output_tokens: 1_000_000,
            cached_input_tokens: 1_000_000,
            ..TokenUsageTotals::default()
        };
        let price = ModelPrice {
            cache_read: None,
            ..PRICE
        };
        assert!((price.cost_usd(&usage) - (15.0 + 3.0)).abs() < 1e-9);
    }

    #[test]
    fn cost_lines_total_only_priced_models_and_name_the_rest() {
        let mut usage = crate::UsageByModel::new();
        usage.insert("anthropic/a".to_string(), totals(1_000_000, 0, 0, 0));
        usage.insert("ollama/b".to_string(), totals(5, 0, 0, 5));
        usage.insert("groq/c".to_string(), totals(5, 0, 0, 5));
        let mut prices = ModelPrices::new();
        prices.insert(
            "anthropic/a".to_string(),
            PriceLookup::Priced {
                price: PRICE,
                source: PriceSource::Catalog,
            },
        );
        prices.insert("ollama/b".to_string(), PriceLookup::Local);
        let text = cost_lines(&usage, &prices).join("\n");
        assert!(text.contains("anthropic/a: $3.00 (catalog)"), "{text}");
        assert!(text.contains("ollama/b: $0 (local)"), "{text}");
        assert!(text.contains("groq/c: unknown price"), "{text}");
        assert!(
            text.contains("Total: $3.00, without 1 model with no price"),
            "{text}"
        );
    }

    #[test]
    fn small_amounts_keep_four_places() {
        assert_eq!(format_usd(0.0), "$0.00");
        assert_eq!(format_usd(0.0042), "$0.0042");
        assert_eq!(format_usd(12.345), "$12.35");
    }
}
