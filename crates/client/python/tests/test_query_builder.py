"""Tests for the query builder (Q and Search classes)."""

from __future__ import annotations

import pytest

from msearchdb.query_builder import Q, Search


# ---------------------------------------------------------------------------
# Q factory tests
# ---------------------------------------------------------------------------

class TestQ:
    """Tests for Q static factory methods."""

    def test_match_default_operator(self):
        result = Q.match("title", "rust programming")
        assert result == {
            "match": {"title": {"query": "rust programming", "operator": "or"}}
        }

    def test_match_and_operator(self):
        result = Q.match("title", "rust programming", operator="and")
        assert result == {
            "match": {"title": {"query": "rust programming", "operator": "and"}}
        }

    def test_match_all(self):
        result = Q.match_all()
        assert result == {"match_all": {}}

    def test_term_string(self):
        result = Q.term("status", "published")
        assert result == {"term": {"status": "published"}}

    def test_term_boolean(self):
        result = Q.term("in_stock", True)
        assert result == {"term": {"in_stock": True}}

    def test_term_numeric(self):
        result = Q.term("count", 42)
        assert result == {"term": {"count": 42}}

    def test_range_gte_lte(self):
        result = Q.range("price", gte=10.0, lte=100.0)
        assert result == {"range": {"price": {"gte": 10.0, "lte": 100.0}}}

    def test_range_gt_lt(self):
        result = Q.range("price", gt=10, lt=100)
        assert result == {"range": {"price": {"gt": 10, "lt": 100}}}

    def test_range_partial(self):
        result = Q.range("price", gte=50)
        assert result == {"range": {"price": {"gte": 50}}}

    def test_range_empty(self):
        result = Q.range("price")
        assert result == {"range": {"price": {}}}

    def test_bool_must_only(self):
        result = Q.bool(must=[Q.match("title", "rust")])
        assert result == {
            "bool": {"must": [{"match": {"title": {"query": "rust", "operator": "or"}}}]}
        }

    def test_bool_should_only(self):
        result = Q.bool(should=[Q.term("status", "a"), Q.term("status", "b")])
        assert result == {
            "bool": {
                "should": [
                    {"term": {"status": "a"}},
                    {"term": {"status": "b"}},
                ]
            }
        }

    def test_bool_must_not(self):
        result = Q.bool(must_not=[Q.term("status", "deleted")])
        assert result == {
            "bool": {"must_not": [{"term": {"status": "deleted"}}]}
        }

    def test_bool_combined(self):
        result = Q.bool(
            must=[Q.match("title", "rust")],
            should=[Q.term("tag", "tutorial")],
            must_not=[Q.term("status", "draft")],
        )
        assert "must" in result["bool"]
        assert "should" in result["bool"]
        assert "must_not" in result["bool"]

    def test_bool_with_filter(self):
        result = Q.bool(
            must=[Q.match("title", "rust")],
            filter=[Q.range("price", lte=100)],
        )
        # filter clauses are appended to must
        must_clauses = result["bool"]["must"]
        assert len(must_clauses) == 2
        assert {"range": {"price": {"lte": 100}}} in must_clauses

    def test_bool_filter_without_must(self):
        result = Q.bool(filter=[Q.term("status", "active")])
        # filter creates must
        assert "must" in result["bool"]
        assert Q.term("status", "active") in result["bool"]["must"]

    def test_bool_minimum_should_match(self):
        result = Q.bool(
            should=[Q.term("a", 1), Q.term("b", 2)],
            minimum_should_match=1,
        )
        assert result["bool"]["minimum_should_match"] == 1

    def test_fuzzy_default_fuzziness(self):
        result = Q.fuzzy("name", "lptp")
        assert result == {"fuzzy": {"name": {"value": "lptp", "fuzziness": 1}}}

    def test_fuzzy_custom_fuzziness(self):
        result = Q.fuzzy("name", "lptp", fuzziness=2)
        assert result == {"fuzzy": {"name": {"value": "lptp", "fuzziness": 2}}}

    def test_phrase(self):
        result = Q.phrase("title", "rust programming")
        # Phrase is implemented as match with operator=and
        assert result == {
            "match": {"title": {"query": "rust programming", "operator": "and"}}
        }

    def test_wildcard(self):
        result = Q.wildcard("name", "lap*")
        assert result == {"wildcard": {"name": "lap*"}}


# ---------------------------------------------------------------------------
# Search builder tests
# ---------------------------------------------------------------------------

class TestSearch:
    """Tests for the Search chainable builder."""

    def test_empty_search(self):
        result = Search().to_dict()
        assert result == {}

    def test_query_only(self):
        result = Search().query(Q.match_all()).to_dict()
        assert result == {"query": {"match_all": {}}}

    def test_size(self):
        result = Search().size(20).to_dict()
        assert result == {"size": 20}

    def test_from(self):
        result = Search().from_(10).to_dict()
        assert result == {"from_": 10}

    def test_sort_single(self):
        result = Search().sort("price", "asc").to_dict()
        assert result == {"sort": [{"field": "price", "order": "asc"}]}

    def test_sort_multiple(self):
        result = Search().sort("date", "desc").sort("price", "asc").to_dict()
        assert result == {
            "sort": [
                {"field": "date", "order": "desc"},
                {"field": "price", "order": "asc"},
            ]
        }

    def test_sort_default_order(self):
        result = Search().sort("score").to_dict()
        assert result == {"sort": [{"field": "score", "order": "desc"}]}

    def test_highlight_single(self):
        result = Search().highlight("title").to_dict()
        assert result == {"highlight": {"fields": ["title"]}}

    def test_highlight_multiple(self):
        result = Search().highlight("title", "body").to_dict()
        assert result == {"highlight": {"fields": ["title", "body"]}}

    def test_highlight_custom_tags(self):
        result = Search().highlight("title", pre_tag="<b>", post_tag="</b>").to_dict()
        assert result == {
            "highlight": {
                "fields": ["title"],
                "pre_tag": "<b>",
                "post_tag": "</b>",
            }
        }

    def test_full_chain(self):
        result = (
            Search()
            .query(Q.bool(
                must=[Q.match("title", "rust")],
                filter=[Q.range("price", lte=100)],
            ))
            .size(5)
            .from_(10)
            .sort("price", "asc")
            .highlight("title", "body")
            .to_dict()
        )
        assert "query" in result
        assert result["size"] == 5
        assert result["from_"] == 10
        assert len(result["sort"]) == 1
        assert result["highlight"]["fields"] == ["title", "body"]

    def test_chaining_returns_self(self):
        s = Search()
        assert s.query(Q.match_all()) is s
        assert s.size(10) is s
        assert s.from_(0) is s
        assert s.sort("f") is s
        assert s.highlight("f") is s
