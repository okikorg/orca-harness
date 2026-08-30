from mcp.server.fastmcp import FastMCP

mcp = FastMCP("{{NAME}}")


def echo(text: str) -> str:
    return text


mcp.tool()(echo)


def main() -> None:
    mcp.run(transport="stdio")


if __name__ == "__main__":
    main()
