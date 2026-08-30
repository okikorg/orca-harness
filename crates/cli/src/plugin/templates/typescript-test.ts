import { describe, expect, it } from "vitest";
import { echo } from "../src/index.js";

describe("echo", () => {
  it("returns its input", () => {
    expect(echo("hello")).toBe("hello");
  });
});
