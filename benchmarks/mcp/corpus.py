"""Sanitized real MCP catalog snapshot and labeled search-query cases.

Captured from this workspace's enabled AWS Docs and read-only GitHub MCP
servers. No server commands, credentials, or input schemas are stored here.
"""

from dataclasses import dataclass


@dataclass(frozen=True)
class Tool:
    key: str
    server: str
    name: str
    description: str


@dataclass(frozen=True)
class Case:
    query: str
    target: str
    quality: str


TOOLS = [
    Tool("aws_read", "awsdocs", "aws read documentation", "Fetch full AWS doc pages as markdown"),
    Tool("aws_search", "awsdocs", "aws search documentation", "AWS docs search"),
    Tool("aws_regions", "awsdocs", "aws list regions", "Retrieve a list of all AWS regions"),
    Tool("aws_availability", "awsdocs", "aws get regional availability", "AWS resource availability per region"),
    Tool("aws_skill", "awsdocs", "aws retrieve skill", "Retrieve an AWS skill workflows references"),
    Tool("gh_commit", "github", "get commit", "Get details for a commit from a GitHub repository"),
    Tool("gh_file", "github", "get file contents", "Get the contents of a file or directory from a GitHub repository"),
    Tool("gh_latest_release", "github", "get latest release", "Get the latest release in a GitHub repository"),
    Tool("gh_release_tag", "github", "get release by tag", "Get a specific release by its tag name in a GitHub repository"),
    Tool("gh_tag", "github", "get tag", "Get details about a specific git tag in a GitHub repository"),
    Tool("gh_branches", "github", "list branches", "List branches in a GitHub repository"),
    Tool("gh_commits", "github", "list commits", "Get list of commits of a branch in a GitHub repository"),
    Tool("gh_collaborators", "github", "list repository collaborators", "List collaborators of a GitHub repository"),
    Tool("gh_releases", "github", "list releases", "List releases in a GitHub repository"),
    Tool("gh_tags", "github", "list tags", "List git tags in a GitHub repository"),
    Tool("gh_code", "github", "search code", "Fast precise code search across GitHub repositories for exact symbols functions classes patterns"),
    Tool("gh_search_commits", "github", "search commits", "Search commits across GitHub repositories for changes authors messages"),
    Tool("gh_repositories", "github", "search repositories", "Find GitHub repositories by name description readme topics metadata projects examples"),
]

CASES = [
    Case("read aws documentation", "aws_read", "deliberate"),
    Case("search aws documentation", "aws_search", "deliberate"),
    Case("list aws regions", "aws_regions", "deliberate"),
    Case("regional availability", "aws_availability", "deliberate"),
    Case("retrieve aws skill", "aws_skill", "deliberate"),
    Case("get commit details", "gh_commit", "deliberate"),
    Case("file contents directory", "gh_file", "deliberate"),
    Case("latest release", "gh_latest_release", "deliberate"),
    Case("release tag name", "gh_release_tag", "deliberate"),
    Case("specific git tag", "gh_tag", "deliberate"),
    Case("list branches", "gh_branches", "deliberate"),
    Case("list commits branch", "gh_commits", "deliberate"),
    Case("list collaborators", "gh_collaborators", "deliberate"),
    Case("list releases", "gh_releases", "deliberate"),
    Case("list git tags", "gh_tags", "deliberate"),
    Case("search exact code symbols", "gh_code", "deliberate"),
    Case("search commits authors", "gh_search_commits", "deliberate"),
    Case("find repositories topics", "gh_repositories", "deliberate"),
    Case("aws docs", "aws_search", "short"),
    Case("aws regions", "aws_regions", "short"),
    Case("aws resource", "aws_availability", "short"),
    Case("github commit", "gh_commit", "short"),
    Case("github file", "gh_file", "short"),
    Case("github release", "gh_latest_release", "short"),
    Case("github release", "gh_release_tag", "short"),
    Case("git tag", "gh_tag", "short"),
    Case("github branches", "gh_branches", "short"),
    Case("github commits", "gh_commits", "short"),
    Case("github collaborators", "gh_collaborators", "short"),
    Case("github releases", "gh_releases", "short"),
    Case("github tags", "gh_tags", "short"),
    Case("github code search", "gh_code", "short"),
    Case("github commit search", "gh_search_commits", "short"),
    Case("github repositories", "gh_repositories", "short"),
    Case("repository", "gh_repositories", "underspecified"),
    Case("release", "gh_releases", "underspecified"),
    Case("commit", "gh_search_commits", "underspecified"),
    Case("tag", "gh_tags", "underspecified"),
]


def tokens(value: str) -> list[str]:
    current = []
    output = []
    for character in value.lower():
        if character.isalnum():
            current.append(character)
        elif current:
            output.append("".join(current))
            current = []
    if current:
        output.append("".join(current))
    return output


def singular(token: str) -> str:
    if len(token) > 4 and token.endswith("ies"):
        return token[:-3] + "y"
    if len(token) > 4 and token.endswith(("ches", "shes", "sses", "xes", "zes")):
        return token[:-2]
    if len(token) > 3 and token.endswith("s") and not token.endswith(("ss", "us", "is")):
        return token[:-1]
    return token


def term_match(term: str, field: list[str], normalize_plural: bool) -> int:
    if term in field:
        return 3
    if any(term in token for token in field):
        return 2
    if normalize_plural and any(singular(term) == singular(token) for token in field):
        return 1
    return 0


def relevance(query: str, tool: Tool) -> int | None:
    terms = tokens(query)
    fields = [tokens(tool.name), tokens(tool.description), tokens(tool.server)]
    matched, score = match_score(terms, fields)
    if matched != len(terms):
        return None

    normalized_terms = [singular(term) for term in terms]
    normalized_name = [singular(term) for term in fields[0]]
    if normalized_terms == normalized_name:
        score += 100
    elif all(term in normalized_name for term in normalized_terms):
        score += 20
    return score


def partial_relevance(query: str, tool: Tool) -> int | None:
    terms = tokens(query)
    fields = [tokens(tool.name), tokens(tool.description), tokens(tool.server)]
    matched, score = match_score(terms, fields)
    if matched == 0:
        return None
    score += matched * 100 // len(terms)

    normalized_terms = [singular(term) for term in terms]
    normalized_name = [singular(term) for term in fields[0]]
    if all(term in normalized_terms for term in normalized_name):
        score += 20
    return score


def match_score(terms: list[str], fields: list[list[str]]) -> tuple[int, int]:
    matched = 0
    score = 0
    for term in terms:
        best = max(
            term_match(term, field, index == 0) * weight
            for index, (field, weight) in enumerate(zip(fields, (8, 4, 2)))
        )
        matched += best > 0
        score += best
    return matched, score


def results(query: str) -> list[Tool]:
    ranked = []
    fallback = []
    for order, tool in enumerate(TOOLS):
        score = relevance(query, tool)
        if score is not None:
            ranked.append((-score, len(tokens(tool.name)), order, tool))
        else:
            score = partial_relevance(query, tool)
            if score is not None:
                fallback.append((-score, len(tokens(tool.name)), order, tool))
    if not ranked:
        ranked = fallback
    ranked.sort(key=lambda item: item[:3])
    return [tool for _, _, _, tool in ranked]


def rows():
    measured = []
    for case in CASES:
        found = results(case.query)
        keys = [tool.key for tool in found]
        rank = keys.index(case.target) + 1 if case.target in keys else None
        measured.append((case, rank, len(found)))
    return measured
