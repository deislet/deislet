//! Reading the two files a static site configures itself with.
//!
//! `_redirects` and `_headers` sit at the root of the assets directory, are
//! read here, and are *not* carried into the bundle as files - they describe
//! the site, they are not part of it, and serving them would publish an
//! author's routing table to anyone who asked for `/_redirects`.
//!
//! Everything is checked now, while the author still has the file open. A rule
//! that does not parse, a status that means nothing, a target that names a file
//! this bundle does not contain: all of them fail the build with the file, the
//! line and what was wrong. The alternative - skipping a bad line at request
//! time - hides the mistake from the only person who can fix it and shows it to
//! a visitor instead.
//!
//! The format itself lives in `deis_bundle`, next to the asset index that
//! applies it. This module is the part that only a build can do: find the
//! files, and check the rules against the files that are actually there.

use anyhow::{bail, Context, Result};
use deis_build_core::ProjectConfig;
use deis_bundle::{
    parse_headers, parse_redirects, redirects, Asset, AssetIndex, HeaderTable, NotFound,
    RedirectTable, SiteConfig, HEADERS_FILE, NOT_FOUND_PAGE, REDIRECTS_FILE, SPA_SHELL,
};

use crate::assets::StagedAsset;

/// Whether a staged file is read by the build rather than served to visitors.
///
/// Only at the root: a `docs/_redirects` is a file a documentation site is
/// writing *about*, and swallowing it would be a surprise with no way to undo.
pub fn is_site_file(path: &str) -> bool {
    path == REDIRECTS_FILE || path == HEADERS_FILE
}

/// Read `_redirects` and `_headers`, settle the not-found behaviour, and check
/// all three against the files the bundle will actually carry.
pub fn build_site_config(config: &ProjectConfig, assets: &[StagedAsset]) -> Result<SiteConfig> {
    let redirects_text = text_of(assets, REDIRECTS_FILE)?;
    let headers_text = text_of(assets, HEADERS_FILE)?;

    let parsed = match &redirects_text {
        Some(text) => parse_redirects(text)?,
        None => Default::default(),
    };
    let header_rules = match &headers_text {
        Some(text) => parse_headers(text)?,
        None => Vec::new(),
    };

    let served: Vec<Asset> = assets
        .iter()
        .filter(|asset| !is_site_file(&asset.path))
        .map(|asset| Asset {
            path: asset.path.clone(),
            sha256: None,
            content_type: None,
            size: None,
        })
        .collect();
    let index = AssetIndex::build(&served).0;

    // A target built out of captures cannot be checked without guessing what
    // it will expand to; a fixed one can, and a typo there is the most common
    // way a redirect table ends up pointing at nothing.
    for (rule, target) in redirects::literal_asset_targets(&parsed.rules) {
        if index.lookup(&target).is_none() {
            let source = &parsed.rules[rule];
            bail!(
                "{REDIRECTS_FILE} line {}: {:?} sends {:?} to {target:?}, which is not a file in \
                 this bundle; the rule would match and then serve nothing",
                parsed.line_of(rule),
                format!("{} {} {}", source.from, source.to, source.status),
                source.from
            );
        }
    }

    let not_found = not_found_for(config, &index)?;

    let site = SiteConfig {
        redirects: parsed.rules,
        headers: header_rules,
        not_found,
    };

    // The same compile a node runs when it loads the bundle. Running it here
    // means a bundle that would be refused at load is refused at build, which
    // is the difference between a failed build and a failed deploy.
    RedirectTable::compile(&site.redirects)?;
    HeaderTable::compile(&site.headers)?;

    Ok(site)
}

/// What to answer for a path nothing claimed.
///
/// Configured wins. Unconfigured infers `404-page` from the presence of
/// `404.html`, because that is what shipping that file means on every other
/// static host - an author who put one there and got the isolate's plain-text
/// "Not Found" would reasonably call that a bug. Inferring the SPA behaviour is
/// not possible in the same way: every site has an `index.html`, and only the
/// author knows whether an unmatched path is a client-side route or a mistake.
fn not_found_for(config: &ProjectConfig, index: &AssetIndex) -> Result<NotFound> {
    let declared = config
        .assets
        .as_ref()
        .and_then(|assets| assets.not_found.as_deref());

    let chosen = match declared {
        Some(text) => NotFound::parse(text)
            .map_err(|e| anyhow::anyhow!("the project's \"assets.notFound\" is not usable: {e}"))?,
        None => {
            if index.lookup(&format!("/{NOT_FOUND_PAGE}")).is_some() {
                NotFound::Page
            } else {
                NotFound::None
            }
        }
    };

    // Only a declared choice can name a file that is not there; the inferred
    // one is inferred from the file existing.
    if declared.is_some() {
        if let Some(needed) = chosen.asset() {
            if index.lookup(&format!("/{needed}")).is_none() {
                bail!(
                    "the project asks for \"assets.notFound\": {:?}, which serves {needed:?}, and \
                     the assets directory has no such file",
                    declared.unwrap_or_default()
                );
            }
        }
    }

    if chosen == NotFound::SinglePageApp && index.lookup(&format!("/{SPA_SHELL}")).is_none() {
        bail!("a single-page app fallback serves {SPA_SHELL:?}, which this bundle does not carry");
    }

    Ok(chosen)
}

/// The contents of one staged file, as text.
fn text_of(assets: &[StagedAsset], name: &str) -> Result<Option<String>> {
    let Some(asset) = assets.iter().find(|asset| asset.path == name) else {
        return Ok(None);
    };
    let text = String::from_utf8(asset.bytes.clone())
        .with_context(|| format!("{name} has to be UTF-8 text; this one is not"))?;
    Ok(Some(text))
}

#[cfg(test)]
#[path = "site_test.rs"]
mod site_test;
