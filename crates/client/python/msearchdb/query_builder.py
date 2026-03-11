"""Fluent query DSL builder for MSearchDB searches.

Provides two main components:

- :class:`Q` — static factory methods that produce query dicts compatible with
  the MSearchDB query DSL.
- :class:`Search` — a chainable builder that assembles a full search request
  body including query, size, pagination, sorting, and highlighting.

Example::

    from msearchdb import Q, Search

    search_body = (
        Search()
        .query(Q.bool(
            must=[Q.match("title", "python programming")],
            filter=[Q.range("year", gte=2020)],
        ))
        .size(20)
        .sort("year", "desc")
        .highlight("title", "body")
        .to_dict()
    )
"""

from __future__ import annotations

from typing import Any


# ---------------------------------------------------------------------------
# Q — static query factories
# ---------------------------------------------------------------------------

class Q:
    """Static factory methods that build MSearchDB query DSL dicts."""

    @staticmethod
    def match(field: str, query: str, operator: str = "or") -> dict[str, Any]:
        """Full-text match query.

        Args:
            field: The field to search.
            query: The search text.
            operator: ``"or"`` (default) or ``"and"``.
        """
        return {"match": {field: {"query": query, "operator": operator}}}

    @staticmethod
    def match_all() -> dict[str, Any]:
        """Match all documents."""
        return {"match_all": {}}

    @staticmethod
    def term(field: str, value: Any) -> dict[str, Any]:
        """Exact-value term query.

        Args:
            field: The field to match.
            value: The exact value (string, number, or bool).
        """
        return {"term": {field: value}}

    @staticmethod
    def range(
        field: str,
        *,
        gte: float | int | None = None,
        lte: float | int | None = None,
        gt: float | int | None = None,
        lt: float | int | None = None,
    ) -> dict[str, Any]:
        """Numeric range query.

        Args:
            field: The numeric field.
            gte: Greater-than-or-equal.
            lte: Less-than-or-equal.
            gt: Greater-than (strict).
            lt: Less-than (strict).
        """
        conditions: dict[str, float | int] = {}
        if gte is not None:
            conditions["gte"] = gte
        if lte is not None:
            conditions["lte"] = lte
        if gt is not None:
            conditions["gt"] = gt
        if lt is not None:
            conditions["lt"] = lt
        return {"range": {field: conditions}}

    @staticmethod
    def bool(
        must: list[dict[str, Any]] | None = None,
        should: list[dict[str, Any]] | None = None,
        must_not: list[dict[str, Any]] | None = None,
        filter: list[dict[str, Any]] | None = None,
        minimum_should_match: int | None = None,
    ) -> dict[str, Any]:
        """Boolean compound query.

        Args:
            must: Clauses that **must** all match (AND).
            should: Clauses where **at least one** should match (OR).
            must_not: Clauses that **must not** match (NOT).
            filter: Clauses that must match but don't affect scoring.
            minimum_should_match: Minimum number of ``should`` clauses to match.
        """
        clause: dict[str, Any] = {}
        if must:
            clause["must"] = must
        if should:
            clause["should"] = should
        if must_not:
            clause["must_not"] = must_not
        if filter:
            # Server treats filter same as must for now; include for API compat
            clause.setdefault("must", []).extend(filter)
        if minimum_should_match is not None:
            clause["minimum_should_match"] = minimum_should_match
        return {"bool": clause}

    @staticmethod
    def fuzzy(field: str, value: str, fuzziness: int = 1) -> dict[str, Any]:
        """Fuzzy query for approximate string matching.

        Args:
            field: The field to search.
            value: The (possibly misspelled) search term.
            fuzziness: Max edit distance (0, 1, or 2).
        """
        return {"fuzzy": {field: {"value": value, "fuzziness": fuzziness}}}

    @staticmethod
    def phrase(field: str, phrase: str) -> dict[str, Any]:
        """Phrase match query (exact phrase in order).

        Args:
            field: The field to search.
            phrase: The exact phrase to match.
        """
        return {"match": {field: {"query": phrase, "operator": "and"}}}

    @staticmethod
    def wildcard(field: str, pattern: str) -> dict[str, Any]:
        """Wildcard pattern query (``*`` and ``?`` supported).

        Args:
            field: The field to search.
            pattern: Wildcard pattern (e.g. ``"lap*"``).
        """
        return {"wildcard": {field: pattern}}


# ---------------------------------------------------------------------------
# Search — full search request builder
# ---------------------------------------------------------------------------

class Search:
    """Chainable search request builder.

    Example::

        body = (
            Search()
            .query(Q.match("title", "rust"))
            .size(5)
            .from_(10)
            .sort("date", "desc")
            .highlight("title")
            .to_dict()
        )
    """

    def __init__(self) -> None:
        self._query: dict[str, Any] | None = None
        self._size: int | None = None
        self._from: int | None = None
        self._sort: list[dict[str, str]] = []
        self._highlight_fields: list[str] = []
        self._highlight_pre_tag: str | None = None
        self._highlight_post_tag: str | None = None

    # -- Chainable setters ---------------------------------------------------

    def query(self, q: dict[str, Any]) -> Search:
        """Set the query clause."""
        self._query = q
        return self

    def size(self, n: int) -> Search:
        """Maximum number of hits to return."""
        self._size = n
        return self

    def from_(self, n: int) -> Search:
        """Starting offset for pagination."""
        self._from = n
        return self

    def sort(self, field: str, order: str = "desc") -> Search:
        """Add a sort clause.

        Can be called multiple times for multi-field sorting.
        """
        self._sort.append({"field": field, "order": order})
        return self

    def highlight(self, *fields: str, pre_tag: str | None = None, post_tag: str | None = None) -> Search:
        """Enable hit highlighting for one or more fields.

        Args:
            *fields: Field names to highlight.
            pre_tag: Custom pre-highlight tag (default ``<em>``).
            post_tag: Custom post-highlight tag (default ``</em>``).
        """
        self._highlight_fields.extend(fields)
        if pre_tag is not None:
            self._highlight_pre_tag = pre_tag
        if post_tag is not None:
            self._highlight_post_tag = post_tag
        return self

    # -- Serialisation -------------------------------------------------------

    def to_dict(self) -> dict[str, Any]:
        """Build the search request body as a dict.

        The returned dict can be unpacked into
        :meth:`MSearchDB.search() <msearchdb.client.MSearchDB.search>` as
        keyword arguments::

            body = Search().query(Q.match("f", "v")).to_dict()
            results = db.search("my_collection", **body)
        """
        body: dict[str, Any] = {}

        if self._query is not None:
            body["query"] = self._query

        if self._size is not None:
            body["size"] = self._size

        if self._from is not None:
            body["from_"] = self._from

        if self._sort:
            body["sort"] = self._sort

        if self._highlight_fields:
            hl: dict[str, Any] = {"fields": list(self._highlight_fields)}
            if self._highlight_pre_tag is not None:
                hl["pre_tag"] = self._highlight_pre_tag
            if self._highlight_post_tag is not None:
                hl["post_tag"] = self._highlight_post_tag
            body["highlight"] = hl

        return body
