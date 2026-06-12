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
        action: "Close modal/search or focus Files",
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
        action: "Search files or filter packages",
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
        action: "Scan missing visible directory sizes",
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
        action: "Move selected item to Trash",
        footer: None,
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
        footer: None,
    },
    KeyBinding {
        key: "x",
        action: "Uninstall selected package",
        footer: None,
    },
];

const RECLAIM_REPORTS: &[KeyBinding] = &[
    KeyBinding {
        key: "R",
        action: "Re-scan reclaim pane",
        footer: None,
    },
    KeyBinding {
        key: "E",
        action: "Empty Trash from Reclaim pane",
        footer: None,
    },
    KeyBinding {
        key: "d",
        action: "Trash selected reclaim/top-files path",
        footer: None,
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

pub fn footer_bindings() -> impl Iterator<Item = FooterBinding> {
    HELP_SECTIONS
        .iter()
        .flat_map(|section| section.bindings.iter())
        .filter_map(|binding| binding.footer)
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
    fn footer_advertises_help_binding() {
        assert!(footer_bindings().any(|binding| binding.key == "?"));
    }
}
