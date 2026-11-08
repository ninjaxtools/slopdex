use super::*;

#[test]
fn markdown_rename_updates_inline_images_and_reference_definitions() -> Result<()> {
    let f = Fixture::new(&[
        ("docs/old.md", "# Guide\n"),
        (
            "README.md",
            "[inline](docs/old.md#guide \"A title\")\n![image](./docs/old.md?raw=1#guide)\n[reference][guide] and [guide][] and [guide]\n\n[guide]: <docs/old.md#guide> 'Another title'\n",
        ),
        ("docs/deep/index.markdown", "[guide](../old.md)\n"),
        ("docs/unrelated.md", "[other](other.md)\n"),
    ])?;
    let plan = f.plan("docs/old.md", "docs/new.markdown")?;
    assert_eq!(plan.changes.len(), 4);
    assert_eq!(f.read("docs/old.md"), "# Guide\n");
    assert!(!f.root.join("docs/new.markdown").exists());
    plan.apply()?;
    assert!(!f.root.join("docs/old.md").exists());
    assert_eq!(f.read("docs/new.markdown"), "# Guide\n");
    assert_eq!(
        f.read("README.md"),
        "[inline](docs/new.markdown#guide \"A title\")\n![image](./docs/new.markdown?raw=1#guide)\n[reference][guide] and [guide][] and [guide]\n\n[guide]: <docs/new.markdown#guide> 'Another title'\n"
    );
    assert_eq!(
        f.read("docs/deep/index.markdown"),
        "[guide](../new.markdown)\n"
    );
    assert_eq!(f.read("docs/unrelated.md"), "[other](other.md)\n");
    Ok(())
}

#[test]
fn markdown_move_rebases_outgoing_links_and_preserves_self_fragments() -> Result<()> {
    let f = Fixture::new(&[
        (
            "docs/guide.md",
            "# Guide\n[other](other.md#topic)\n![asset](../assets/logo.png)\n[self](./guide.md#guide)\n[fragment](#guide)\n[query](?raw=1)\n[root](/docs/guide.md#guide)\n[dir](nested/)\n[reference][other]\n\n[other]: other.md \"Other\"\n",
        ),
        ("README.md", "[guide](/docs/guide.md?raw=1#guide)\n"),
    ])?;
    f.plan("docs/guide.md", "docs/nested/new.md")?.apply()?;
    assert_eq!(
        f.read("docs/nested/new.md"),
        "# Guide\n[other](../other.md#topic)\n![asset](../../assets/logo.png)\n[self](./new.md#guide)\n[fragment](#guide)\n[query](?raw=1)\n[root](/docs/nested/new.md#guide)\n[dir](./)\n[reference][other]\n\n[other]: ../other.md \"Other\"\n"
    );
    assert_eq!(
        f.read("README.md"),
        "[guide](/docs/nested/new.md?raw=1#guide)\n"
    );
    Ok(())
}

#[test]
fn markdown_links_in_containers_headings_and_tables_are_updated() -> Result<()> {
    let f = Fixture::new(&[
        ("old.md", "# Old\n"),
        (
            "README.md",
            "# [Guide](old.md)\n\n> [Guide](old.md)\n>\n> [ref]: old.md\n\n- [Guide](old.md)\n\n| Guide |\n| --- |\n| [Guide](old.md) |\n",
        ),
    ])?;
    f.plan("old.md", "new.md")?.apply()?;
    assert_eq!(
        f.read("README.md"),
        "# [Guide](new.md)\n\n> [Guide](new.md)\n>\n> [ref]: new.md\n\n- [Guide](new.md)\n\n| Guide |\n| --- |\n| [Guide](new.md) |\n"
    );
    Ok(())
}

#[test]
fn markdown_preserves_prose_code_external_urls_and_unrelated_links() -> Result<()> {
    let unchanged = "old.md\n`[code](old.md)`\n\n```md\n[fenced](old.md)\n[ref]: old.md\n```\n\n    [indented](old.md)\n\n<!-- [comment](old.md) -->\n\n[external](https://example.com/old.md)\n[network](//example.com/old.md)\n[mail](mailto:old.md)\n[other](other/old.md)\n\\[escaped](old.md)\n";
    let f = Fixture::new(&[("old.md", "# Old\n"), ("README.md", unchanged)])?;
    f.plan("old.md", "new.md")?.apply()?;
    assert_eq!(f.read("README.md"), unchanged);
    Ok(())
}

#[test]
fn markdown_handles_escaped_and_percent_encoded_paths_without_changing_line_endings() -> Result<()>
{
    let f = Fixture::new(&[
        ("docs/old (guide).md", "# Guide\r\n"),
        (
            "README.md",
            "[escaped](docs/old%20\\(guide\\).md#topic)\r\n[angle](<docs/old (guide).md> \"Title\")\r\n",
        ),
    ])?;
    f.plan("docs/old (guide).md", "new docs/日本語 #guide.MD")?
        .apply()?;
    assert_eq!(f.read("new docs/日本語 #guide.MD"), "# Guide\r\n");
    assert_eq!(
        f.read("README.md"),
        "[escaped](new%20docs/%E6%97%A5%E6%9C%AC%E8%AA%9E%20%23guide.MD#topic)\r\n[angle](<new%20docs/%E6%97%A5%E6%9C%AC%E8%AA%9E%20%23guide.MD> \"Title\")\r\n"
    );
    Ok(())
}

#[test]
fn markdown_decodes_entities_and_preserves_entity_encoded_fragments() -> Result<()> {
    let f = Fixture::new(&[
        ("docs/a&b.md", "[other](c&amp;d.md)\n"),
        (
            "README.md",
            "[named](docs/a&amp;b.md&#35;topic)\n[numeric](docs/a&#38;b.md?x=1&amp;y=2)\n",
        ),
    ])?;
    f.plan("docs/a&b.md", "guides/new.md")?.apply()?;
    assert_eq!(f.read("guides/new.md"), "[other](../docs/c%26d.md)\n");
    assert_eq!(
        f.read("README.md"),
        "[named](guides/new.md&#35;topic)\n[numeric](guides/new.md?x=1&amp;y=2)\n"
    );
    Ok(())
}

#[test]
fn markdown_rejects_conflicts_symbols_and_cross_language_moves() -> Result<()> {
    let f = Fixture::new(&[("old.md", "# Guide\n"), ("existing.md", "# Existing\n")])?;
    for (from, to) in [
        ("old.md", "old.md"),
        ("old.md", "existing.md"),
        ("old.md", "new.ts"),
        ("old.md", "new.md::Guide"),
        ("old.md::Guide", "new.md::New"),
        ("old.md", "../outside.md"),
    ] {
        assert!(f.plan(from, to).is_err(), "{from} -> {to}");
    }
    assert_eq!(f.read("old.md"), "# Guide\n");
    Ok(())
}

#[test]
fn markdown_validates_unchanged_consulted_files_before_applying() -> Result<()> {
    let f = Fixture::new(&[("old.md", "# Old\n"), ("README.md", "No links yet.\n")])?;
    let plan = f.plan("old.md", "new.md")?;
    fs::write(f.root.join("README.md"), "[new link](old.md)\n")?;
    assert!(plan.apply().is_err());
    assert_eq!(f.read("old.md"), "# Old\n");
    assert!(!f.root.join("new.md").exists());
    Ok(())
}
