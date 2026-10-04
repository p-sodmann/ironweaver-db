"""Step 14: the query methods (documentation/python-api.md, "Queries"),
against an embedded store and a server."""

import pytest

import iwdb
from iwdb import attr, const, edge_type, label


@pytest.fixture
def graph(store):
    """a -> b -> c -> d -> e and a -> e, all "knows" with weight 2 (a -> e:
    10); a to e have label P, ages 20 to 24 and coordinates (i, 0)."""
    with store.transaction() as tx:
        for i, n in enumerate("abcde"):
            tx.upsert_node(n, labels=["P"], attr={"age": 20 + i, "x": i, "y": 0})
        for f, t in ["ab", "bc", "cd", "de"]:
            tx.add_edge(f, t, type="knows", attr={"w": 2.0})
        tx.add_edge("a", "e", type="knows", attr={"w": 10.0})
    return store


def ids(nodes):
    return [n["id"] for n in nodes]


def test_answers_have_the_common_fields(graph):
    a = graph.find(label("P"))
    assert set(a) == {"nodes", "seq", "cursor", "truncated", "work"}
    assert a["seq"] == 1 and a["cursor"] is None and a["truncated"] is False
    assert a["work"]["visited"] >= 5
    assert a["nodes"][0] == {"id": "a", "labels": ["P"], "attr": {"age": 20, "x": 0, "y": 0}, "meta": {}, "version": 1}


def test_find_with_every_operator(graph):
    def find(f):
        return ids(graph.find(f)["nodes"])

    assert find(attr("age") == 22) == ["c"]
    assert find(attr("age") != 22) == ["a", "b", "d", "e"]
    assert find(attr("age") < 22) == ["a", "b"]
    assert find(attr("age") <= 22) == ["a", "b", "c"]
    assert find(attr("age") > 22) == ["d", "e"]
    assert find(attr("age") >= 22) == ["c", "d", "e"]
    assert find(attr("age").isin([20, 24, 99])) == ["a", "e"]
    assert find(attr("age").exists()) == list("abcde")
    assert find(~attr("nope").exists()) == list("abcde")
    assert find((attr("age") < 21) | (attr("age") > 23)) == ["a", "e"]
    assert find(label("P") & (attr("x") == 1)) == ["b"]
    assert find(label("Q")) == []
    assert find(const(True)) == list("abcde")
    assert find(const(False)) == []
    assert find(edge_type("knows")) == []


def test_filters_reject_misuse():
    with pytest.raises(TypeError):
        bool(label("P"))
    with pytest.raises(TypeError):
        label("P") and label("Q")  # noqa: B015
    with pytest.raises(TypeError):
        _ = label("P") & True


def test_bad_filters_raise(graph):
    with pytest.raises(TypeError):
        graph.find({"Label": "P"})
    with pytest.raises(TypeError):
        graph.find(attr("age") == (1, 2))  # a tuple is no value


def test_filters_nest_100_levels(graph):
    # 99 operators around a leaf: 100 levels, the core's limit
    f = label("P")
    for _ in range(99):
        f = ~f
    assert ids(graph.find(f)["nodes"]) == []
    with pytest.raises(ValueError, match="more than 100 levels"):
        graph.find(~f)


def test_values_in_filters_convert_exactly(graph):
    with graph.transaction() as tx:
        tx.upsert_node("z", attr={"tags": ["x", {"k": b"\x00"}]})
    assert ids(graph.find(attr("tags") == ["x", {"k": b"\x00"}])["nodes"]) == ["z"]


def test_pagination(graph):
    first = graph.find(label("P"), max_results=2)
    assert ids(first["nodes"]) == ["a", "b"] and first["cursor"]
    second = graph.find(label("P"), max_results=2, cursor=first["cursor"])
    third = graph.find(label("P"), max_results=2, cursor=second["cursor"])
    assert ids(second["nodes"]) == ["c", "d"] and ids(third["nodes"]) == ["e"]
    assert third["cursor"] is None


def test_a_cursor_expires_after_a_commit(graph):
    first = graph.find(label("P"), max_results=2)
    with graph.transaction() as tx:
        tx.set_attr("a", "n", 1)
    with pytest.raises(iwdb.CursorExpiredError):
        graph.find(label("P"), max_results=2, cursor=first["cursor"])


def test_a_cursor_of_another_request_is_invalid(graph):
    first = graph.find(label("P"), max_results=2)
    with pytest.raises(iwdb.InvalidError):
        graph.find(attr("age") > 0, max_results=2, cursor=first["cursor"])
    with pytest.raises(iwdb.InvalidError):
        graph.find(label("P"), cursor="garbage")


def test_budgets_and_partial_answers(graph):
    with pytest.raises(iwdb.BudgetExceededError):
        graph.find(label("P"), max_visited=2)
    a = graph.find(label("P"), max_visited=2, partial=True)
    assert a["truncated"] is True and a["cursor"] is None
    with pytest.raises(iwdb.InvalidError):
        graph.find(label("P"), max_results=0)
    assert issubclass(iwdb.BudgetExceededError, iwdb.Error)


def test_explain(graph):
    graph.create_index("age")
    e = graph.explain(attr("age") == 22, analyze=True)
    assert e["plan"] == {"kind": "index", "path": ["age"], "lookup": "point"}
    assert e["estimated_candidates"] == 1 and e["candidates"] == 1 and e["nodes"] == 5
    assert graph.explain(attr("age").isin([1, 2]))["plan"]["lookup"] == "in"
    assert graph.explain(attr("x") == 1)["plan"] == {"kind": "scan"}
    assert graph.explain(const(False))["plan"] == {"kind": "empty"}
    assert graph.explain(label("P"))["plan"] == {"kind": "label", "label": "P"}
    union = graph.explain((attr("age") == 1) | label("P"))["plan"]
    assert union["kind"] == "union" and len(union["plans"]) == 2


def test_neighbourhood(graph):
    assert ids(graph.neighbourhood("a")["nodes"]) == ["a", "b", "e"]
    assert ids(graph.neighbourhood(["a"], depth=2)["nodes"]) == ["a", "b", "c", "e"]
    assert ids(graph.neighbourhood("c", direction="in")["nodes"]) == ["b", "c"]
    assert ids(graph.neighbourhood("c", direction="both")["nodes"]) == ["b", "c", "d"]
    assert ids(graph.neighbourhood("a", node_filter=attr("age") > 20)["nodes"]) == ["b", "e"]
    assert ids(graph.neighbourhood("a", edge_filter=attr("w") < 5)["nodes"]) == ["a", "b"]
    assert ids(graph.neighbourhood("a", edge_types=["other"])["nodes"]) == ["a"]
    page = graph.neighbourhood("a", depth=4, max_results=3)
    assert ids(page["nodes"]) == ["a", "b", "c"] and page["cursor"]
    rest = graph.neighbourhood("a", depth=4, max_results=3, cursor=page["cursor"])
    assert ids(rest["nodes"]) == ["d", "e"]
    with pytest.raises(ValueError):
        graph.neighbourhood("a", direction="up")


def test_traverse(graph):
    assert graph.traverse("a")["ids"][0] == "a"
    assert sorted(graph.traverse("a")["ids"]) == list("abcde")
    assert graph.traverse("a", depth=1)["ids"][0] == "a" and len(graph.traverse("a", depth=1)["ids"]) == 3
    assert graph.traverse("e", direction="in", edge_filter=attr("w") < 5)["ids"] == ["e", "d", "c", "b", "a"]
    assert graph.traverse("a", order="dfs", edge_types=["knows"])["ids"][0] == "a"
    with pytest.raises(ValueError):
        graph.traverse("a", order="sideways")


def test_shortest_path(graph):
    assert graph.shortest_path("a", "e")["path"] == {"nodes": ["a", "e"], "cost": 1.0}
    cheap = graph.shortest_path("a", "e", method="dijkstra", weight="w")["path"]
    assert cheap == {"nodes": ["a", "b", "c", "d", "e"], "cost": 8.0}
    assert graph.shortest_path("a", "e", method="astar", weight="w")["path"]["cost"] == 8.0
    assert graph.shortest_path("e", "a")["path"] is None
    assert graph.shortest_path("e", "a", direction="both")["path"]["nodes"] == ["e", "a"]
    assert graph.shortest_path("a", "d", max_depth=2)["path"] is None
    with pytest.raises(iwdb.NotFoundError):
        graph.shortest_path("a", "nobody")
    with pytest.raises(ValueError):
        graph.shortest_path("a", "e", method="teleport")


def test_random_walks(graph):
    walks = graph.random_walks("a", max_length=3, walks=10, seed=7)["walks"]
    assert walks and all(w[0] == "a" and len(w) <= 3 for w in walks)
    assert len({tuple(w) for w in walks}) == len(walks)
    assert all(len(w) >= 3 for w in graph.random_walks("a", max_length=3, walks=5, min_length=3)["walks"])


def test_subgraph(graph):
    s = graph.subgraph("a")
    assert ids(s["nodes"]) == ["a", "b", "e"]
    assert [(e["from"], e["to"]) for e in s["edges"]] == [("a", "b"), ("a", "e")]
    assert s["edges"][0]["type"] == "knows" and s["edges"][0]["attr"] == {"w": 2.0}
    assert {"seq", "cursor", "truncated", "work"} <= set(s)
    assert graph.subgraph("a", edge_filter=attr("w") > 5)["edges"][0]["to"] == "e"


def test_match(graph):
    rows = graph.match("(x:P)-[:knows]->(y)")["rows"]
    assert len(rows) == 5 and rows[0] == {"nodes": ["a", "b"], "edges": [[0]]}
    rows = graph.match("(x:P)-[:knows]->(y)", where={"y": attr("age") > 23, "x": attr("age") < 21})["rows"]
    assert rows == [{"nodes": ["a", "e"], "edges": [[4]]}]
    paths = graph.match("(x {age: 20})-[:knows*2..2]->(y)")["rows"]
    assert paths == [{"nodes": ["a", "c"], "edges": [[0, 1]]}]
    page = graph.match("(x)-[:knows]->(y)", max_results=2)
    rest = graph.match("(x)-[:knows]->(y)", max_results=10, cursor=page["cursor"])
    assert len(page["rows"]) + len(rest["rows"]) == 5
    with pytest.raises(iwdb.InvalidError):
        graph.match("(x")


def test_analyze(graph):
    pr = graph.analyze("pagerank", params={"alpha": 0.9})
    assert [r[0] for r in pr["scores"]][0] == "e" and abs(sum(r[1] for r in pr["scores"]) - 1) < 1e-6
    top = graph.analyze("pagerank", max_results=2)
    assert len(top["scores"]) == 2 and top["truncated"] is True
    assert graph.analyze("degree")["scores"][0] == ["a", 0.5]
    assert graph.analyze("degree", params={"incoming": True})["scores"][0][0] == "e"
    assert graph.analyze("weakly_connected_components")["groups"] == [list("abcde")]
    assert len(graph.analyze("strongly_connected_components")["groups"]) == 5
    assert graph.analyze("leiden", direction="both", params={"seed": 1})["groups"]
    assert graph.analyze("label_propagation", direction="both")["groups"]
    assert graph.analyze("core_number", direction="both")["counts"][0][1] >= 1
    assert graph.analyze("triangles", direction="both")["counts"][0][1] == 0
    assert graph.analyze("pagerank", weight="w")["scores"]
    with pytest.raises(ValueError, match="no parameter 'beta'"):
        graph.analyze("pagerank", params={"beta": 1})
    with pytest.raises(ValueError, match="unknown job"):
        graph.analyze("magic")
    with pytest.raises(iwdb.BudgetExceededError):
        graph.analyze("pagerank", max_visited=2)


def test_queries_on_a_namespace(store):
    store.create_namespace("other")
    ns = store.namespace("other")
    with ns.transaction() as tx:
        tx.upsert_node("x", labels=["P"])
        tx.upsert_node("y")
        tx.add_edge("x", "y")
    assert ids(ns.find(label("P"))["nodes"]) == ["x"]
    assert store.find(label("P"))["nodes"] == []
    assert ns.match("(a)-->(b)")["rows"] == [{"nodes": ["x", "y"], "edges": [[0]]}]
    assert ns.shortest_path("x", "y")["path"]["nodes"] == ["x", "y"]
    assert ns.analyze("weakly_connected_components")["groups"] == [["x", "y"]]
    assert ns.explain(label("P"))["nodes"] == 2
    assert ids(ns.subgraph("x")["nodes"]) == ["x", "y"]
    assert ns.traverse("x")["ids"] == ["x", "y"]
    assert ns.random_walks("x", max_length=2)["walks"] == [["x", "y"]]
    assert ids(ns.neighbourhood("x")["nodes"]) == ["x", "y"]


def test_min_seq_and_timeout(graph):
    seq = graph.seq()
    assert graph.find(label("P"), min_seq=seq)["seq"] == seq
    with pytest.raises(iwdb.TimeoutError):
        graph.find(label("P"), min_seq=seq + 1, timeout=0.05)
