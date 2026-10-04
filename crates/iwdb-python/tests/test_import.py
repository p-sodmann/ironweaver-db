"""Step 13: bulk import and export (ADR 0033, documentation/api/import-export.md)."""

import pytest

import iwdb

LEMON = """\
@nodes
label   coordinates size    title
0       (10,20)     10      "First node"
1       (80,80)     8       "Second node"
2       (20,80)     10      "Third node"
@arcs
        capacity
0   1   10
1   2   20
2   0   8
@attributes
caption "LEMON test digraph"
"""


def test_an_export_imports_back(store, tmp_path):
    with store.transaction() as tx:
        tx.upsert_node("a", labels=["Person"], attr={"name": "Ann", "tags": [1, "x", None]}, meta={"m": 1})
        tx.upsert_node("b", attr={"blob": b"\x00\xff"})
        tx.add_edge("a", "b", type="KNOWS", attr={"w": 0.5})
    store.create_index("name")
    for file, format in [("g.json", "json"), ("g.iwb", "binary")]:
        exported = store.export(tmp_path / file)
        assert (exported["format"], exported["nodes"], exported["edges"]) == (format, 2, 1)
        name = "from_" + format
        report = store.import_namespace(name, tmp_path / file)
        assert (report["name"], report["format"], report["seq"]) == (name, format, 1)
        assert (report["nodes"], report["edges"], report["indexes"], report["dropped"]) == (2, 1, [["name"]], [])
        ns = store.namespace(name)
        a = ns.node("a")
        assert (a["labels"], a["attr"]["tags"], a["meta"], a["version"]) == (["Person"], [1, "x", None], {"m": 1}, 1)
        assert ns.node("b")["attr"]["blob"] == b"\x00\xff"
        assert ns.seq() == 1
        # An export of the namespace imports back to the same file
        again = ns.export(tmp_path / ("again-" + file))
        assert (tmp_path / ("again-" + file)).read_bytes() == (tmp_path / file).read_bytes()
        assert again["bytes"] == (tmp_path / file).stat().st_size
    assert store.namespace("from_json").status()["seq"] == 1


def test_lgf_files_and_errors(store, tmp_path):
    lgf = tmp_path / "graph.lgf"
    lgf.write_text(LEMON)
    report = store.import_namespace("lemon", lgf)
    assert (report["format"], report["nodes"], report["edges"]) == ("lgf", 3, 3)
    assert report["dropped"] == ["@attributes 'caption'"]
    assert store.namespace("lemon").node("1")["attr"] == {"coordinates": "(80,80)", "size": 8, "title": "Second node"}

    with pytest.raises(iwdb.ConflictError):
        store.import_namespace("lemon", lgf)
    bad = tmp_path / "bad.lgf"
    bad.write_text("@nodes\nlabel\na\n@arcs\nw\na b 1\n")
    with pytest.raises(iwdb.InvalidError, match="line 6: no node 'b'"):
        store.import_namespace("bad", bad)
    with pytest.raises(iwdb.InvalidError, match="unknown import format"):
        store.import_namespace("bad", lgf, format="csv")
    with pytest.raises(iwdb.InvalidError, match="invalid import"):
        store.import_namespace("bad", lgf, format="json")
    with pytest.raises(iwdb.IoError):
        store.import_namespace("bad", tmp_path / "missing.lgf")
    with pytest.raises(iwdb.InvalidError, match="unknown export format"):
        store.export(tmp_path / "x", format="lgf")
    assert [n["name"] for n in store.namespaces()] == ["default", "lemon"]


def test_an_import_survives_reopening(path, tmp_path):
    lgf = tmp_path / "graph.lgf"
    lgf.write_text(LEMON)
    with iwdb.Store.open(path) as store:
        store.import_namespace("lemon", lgf)
        with store.namespace("lemon").transaction() as tx:
            tx.upsert_node("3")
        assert tx.result["seq"] == 2
    with iwdb.Store.open(path) as store:
        ns = store.namespace("lemon")
        assert (ns.seq(), ns.node("0")["attr"]["title"], ns.node("3")["id"]) == (2, "First node", "3")
        assert store.status()["recovery"]["namespaces"]["lemon"]["finished_import"] is False


def test_a_merge_upserts_into_an_existing_namespace(store, tmp_path):
    with store.transaction() as tx:
        tx.upsert_node("0", labels=["Old"], attr={"size": 1, "extra": True})
        tx.upsert_node("other")
    seq = store.seq()
    lgf = tmp_path / "graph.lgf"
    lgf.write_text(LEMON)
    report = store.import_file(lgf)
    assert (report["format"], report["nodes"], report["edges"], report["commits"]) == ("lgf", 3, 3, 2)
    assert (report["first_seq"], report["last_seq"]) == (seq + 1, seq + 2)
    node = store.node("0")
    assert node["attr"] == {"coordinates": "(10,20)", "size": 10, "title": "First node"}
    assert node["labels"] == ["Old"] and store.node("other") is not None
    # Again: edges are updated, not added
    edges = store.namespace("default").status()["edges"]
    assert edges == 3
    store.import_file(lgf)
    assert store.namespace("default").status()["edges"] == edges
    store.create_namespace("side")
    side = store.namespace("side")
    assert side.import_file(lgf, format="lgf")["nodes"] == 3
    with pytest.raises(iwdb.InvalidError):
        side.import_file(lgf, format="binary")
