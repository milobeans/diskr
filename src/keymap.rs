use std::collections::HashSet;

use crate::app::Focus;

#[derive(Clone, Copy)]
pub struct FooterBinding {
    pub key: &'static str,
    pub action: &'static str,
}

#[derive(Clone, Copy)]
pub struct KeyBinding {
    pub key: &'static str,
    pub action: &'static str,
    pub footer: Option<FooterBinding>,
}

pub struct KeySection {
    pub title: &'static str,
    pub bindings: &'static [KeyBinding],
}

const GLOBAL: &[KeyBinding] = &[
    KeyBinding {
        key: "?",
        action: "Show keyboard help",
        footer: Some(FooterBinding {
            key: "?",
            action: "help",
        }),
    },
    KeyBinding {
        key: "q",
        action: "Quit",
        footer: Some(FooterBinding {
            key: "q",
            action: "quit",
        }),
    },
    KeyBinding {
        key: "Esc, Ctrl+C",
        action: "Close modal or clear search/filter",
        footer: None,
    },
];

const NAVIGATION: &[KeyBinding] = &[
    KeyBinding {
        key: "Up/Down, j/k",
        action: "Move selection",
        footer: Some(FooterBinding {
            key: "↑↓/jk",
            action: "move",
        }),
    },
    KeyBinding {
        key: "PageUp/PageDown",
        action: "Move one page",
        footer: None,
    },
    KeyBinding {
        key: "Home/End",
        action: "Jump first/last",
        footer: None,
    },
    KeyBinding {
        key: "Enter",
        action: "Open selected item",
        footer: Some(FooterBinding {
            key: "⏎",
            action: "open",
        }),
    },
    KeyBinding {
        key: "Backspace",
        action: "Go to parent directory",
        footer: Some(FooterBinding {
            key: "⌫",
            action: "up",
        }),
    },
    KeyBinding {
        key: "Left/Right, h/l",
        action: "Switch pane or package view",
        footer: Some(FooterBinding {
            key: "←→/hl",
            action: "pane/view",
        }),
    },
    KeyBinding {
        key: "Tab, Shift+Tab",
        action: "Cycle panes",
        footer: Some(FooterBinding {
            key: "Tab",
            action: "pane",
        }),
    },
];

const FILES: &[KeyBinding] = &[
    KeyBinding {
        key: "/",
        action: "Search files or filter packages; Enter keeps, Esc clears",
        footer: Some(FooterBinding {
            key: "/",
            action: "search",
        }),
    },
    KeyBinding {
        key: "i",
        action: "Show details",
        footer: Some(FooterBinding {
            key: "i",
            action: "info",
        }),
    },
    KeyBinding {
        key: ".",
        action: "Toggle hidden files",
        footer: None,
    },
    KeyBinding {
        key: "o",
        action: "Cycle sort mode",
        footer: None,
    },
    KeyBinding {
        key: "r",
        action: "Refresh view and rescan visible dirs",
        footer: Some(FooterBinding {
            key: "r",
            action: "refresh",
        }),
    },
    KeyBinding {
        key: "S",
        action: "Scan missing or stale visible directory sizes",
        footer: Some(FooterBinding {
            key: "S",
            action: "scan all",
        }),
    },
    KeyBinding {
        key: "B",
        action: "Save history baseline",
        footer: None,
    },
    KeyBinding {
        key: "t",
        action: "Open top-files list",
        footer: None,
    },
    KeyBinding {
        key: "c",
        action: "Rename selected file",
        footer: None,
    },
    KeyBinding {
        key: "n",
        action: "Create directory",
        footer: None,
    },
    KeyBinding {
        key: "v",
        action: "Toggle mark",
        footer: None,
    },
    KeyBinding {
        key: "a",
        action: "Mark all visible files",
        footer: None,
    },
    KeyBinding {
        key: "d",
        action: "Move selected item (or marks) to Trash",
        footer: Some(FooterBinding {
            key: "d",
            action: "trash",
        }),
    },
];

const EXTERNAL_ACTIONS: &[KeyBinding] = &[
    KeyBinding {
        key: "Space",
        action: "Quick Look selected item",
        footer: Some(FooterBinding {
            key: "Space",
            action: "preview",
        }),
    },
    KeyBinding {
        key: "f",
        action: "Reveal selected item in Finder",
        footer: None,
    },
    KeyBinding {
        key: "O",
        action: "Open selected item with default app",
        footer: None,
    },
    KeyBinding {
        key: "y",
        action: "Copy selected path",
        footer: None,
    },
    KeyBinding {
        key: "s",
        action: "Open selected location in Terminal",
        footer: None,
    },
];

const PACKAGES: &[KeyBinding] = &[
    KeyBinding {
        key: "p",
        action: "Open packages pane / switch view",
        footer: Some(FooterBinding {
            key: "p",
            action: "packages",
        }),
    },
    KeyBinding {
        key: "u",
        action: "Toggle dependency-leaf filter",
        footer: Some(FooterBinding {
            key: "u",
            action: "leaves",
        }),
    },
    KeyBinding {
        key: "x",
        action: "Uninstall selected package",
        footer: Some(FooterBinding {
            key: "x",
            action: "uninstall",
        }),
    },
];

const RECLAIM_REPORTS: &[KeyBinding] = &[
    KeyBinding {
        key: "R",
        action: "Re-scan reclaim pane",
        footer: Some(FooterBinding {
            key: "R",
            action: "rescan",
        }),
    },
    KeyBinding {
        key: "E",
        action: "Empty Trash (when the report lists Trash)",
        footer: Some(FooterBinding {
            key: "E",
            action: "empty trash",
        }),
    },
    KeyBinding {
        key: "d",
        action: "Trash selected reclaim/top-files path",
        footer: Some(FooterBinding {
            key: "d",
            action: "trash",
        }),
    },
];

pub const HELP_SECTIONS: &[KeySection] = &[
    KeySection {
        title: "Global",
        bindings: GLOBAL,
    },
    KeySection {
        title: "Navigation",
        bindings: NAVIGATION,
    },
    KeySection {
        title: "Files",
        bindings: FILES,
    },
    KeySection {
        title: "Actions",
        bindings: EXTERNAL_ACTIONS,
    },
    KeySection {
        title: "Packages",
        bindings: PACKAGES,
    },
    KeySection {
        title: "Reclaim / Reports",
        bindings: RECLAIM_REPORTS,
    },
];

/// Destructive keys are highlighted differently in the help overlay so they
/// stand out from navigation. Kept here (rather than a struct field) so the
/// single source of truth stays the key table.
pub fn is_destructive_key(key: &str) -> bool {
    matches!(key, "d" | "E" | "x")
}

/// Footer hints relevant to the focused pane, most-relevant first and
/// de-duplicated. The caller renders as many as fit and always advertises `?`,
/// so the strip stays short and on-topic instead of listing every binding.
pub fn footer_bindings_for(focus: Focus) -> Vec<FooterBinding> {
    let sections: &[&[KeyBinding]] = match focus {
        Focus::Files => &[NAVIGATION, FILES, EXTERNAL_ACTIONS, PACKAGES],
        Focus::Disks => &[NAVIGATION, EXTERNAL_ACTIONS, PACKAGES],
        Focus::Packages => &[NAVIGATION, PACKAGES, EXTERNAL_ACTIONS],
        Focus::Reclaim => &[NAVIGATION, RECLAIM_REPORTS, EXTERNAL_ACTIONS],
    };
    let mut seen = HashSet::new();
    let mut out: Vec<FooterBinding> = Vec::new();
    for binding in sections.iter().copied().flatten() {
        if let Some(footer) = binding.footer {
            if seen.insert(footer.key) {
                out.push(footer);
            }
        }
    }
    // `q` quit always rides along after the pane-specific hints; `?` is added
    // by the renderer so it survives width truncation.
    for binding in GLOBAL {
        if let Some(footer) = binding.footer {
            if footer.key != "?" && seen.insert(footer.key) {
                out.push(footer);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_includes_help_binding() {
        assert!(HELP_SECTIONS
            .iter()
            .flat_map(|section| section.bindings.iter())
            .any(|binding| binding.key == "?"));
    }

    #[test]
    fn help_binding_has_a_footer_hint() {
        // The footer renderer always appends `? help`; the `?` binding must
        // carry the footer text it uses.
        let help = GLOBAL.iter().find(|binding| binding.key == "?").unwrap();
        assert_eq!(help.footer.map(|footer| footer.action), Some("help"));
    }

    #[test]
    fn focus_footer_is_focused_and_short() {
        // Files footer carries file actions, not package/reclaim noise.
        let files: Vec<&str> = footer_bindings_for(Focus::Files)
            .iter()
            .map(|fb| fb.key)
            .collect();
        assert!(files.contains(&"/"));
        assert!(files.contains(&"S"));
        assert!(
            !files.contains(&"R"),
            "reclaim key leaked into Files footer"
        );

        // Reclaim footer carries reclaim actions.
        let reclaim: Vec<&str> = footer_bindings_for(Focus::Reclaim)
            .iter()
            .map(|fb| fb.key)
            .collect();
        assert!(reclaim.contains(&"R"));
        assert!(reclaim.contains(&"E"));

        // Packages footer carries package actions.
        let packages: Vec<&str> = footer_bindings_for(Focus::Packages)
            .iter()
            .map(|fb| fb.key)
            .collect();
        assert!(packages.contains(&"p"));
        assert!(packages.contains(&"u"));

        // No focus produces a duplicate key, and `?` is left to the renderer.
        for focus in [Focus::Files, Focus::Disks, Focus::Packages, Focus::Reclaim] {
            let keys: Vec<&str> = footer_bindings_for(focus).iter().map(|fb| fb.key).collect();
            let mut deduped = keys.clone();
            deduped.sort_unstable();
            deduped.dedup();
            assert_eq!(
                keys.len(),
                deduped.len(),
                "duplicate footer key for {focus:?}"
            );
            assert!(!keys.contains(&"?"));
        }
    }

    #[test]
    fn destructive_keys_are_flagged() {
        assert!(is_destructive_key("d"));
        assert!(is_destructive_key("E"));
        assert!(is_destructive_key("x"));
        assert!(!is_destructive_key("r"));
        assert!(!is_destructive_key("S"));
    }

    #[test]
    fn readme_documents_every_keymap_key() {
        let readme = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"));
        for binding in HELP_SECTIONS
            .iter()
            .flat_map(|section| section.bindings.iter())
        {
            assert!(
                readme.contains(binding.key),
                "README key table is missing keymap binding `{}`",
                binding.key
            );
        }
    }

    #[test]
    fn readme_runtime_claims_stay_truthful() {
        let readme = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"));
        assert!(
            !readme.contains("no serde"),
            "README still claims 'no serde'"
        );
        assert!(
            readme.contains("serde_json"),
            "README should name the serde_json dependency"
        );
    }
}
