import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { ModeToggle } from "./ModeToggle";
import type { MetricsSnapshot } from "../types";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
it("reports rejected mode changes and re-enables the control", async () => {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false }));
  render(<ModeToggle latest={{ mode: "demo" } as MetricsSnapshot} />);
  fireEvent.click(screen.getByRole("button"));
  await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("Could not change mode"));
  expect((screen.getByRole("button") as HTMLButtonElement).disabled).toBe(false);
});
it("disables switching until metrics have arrived", () => {
  render(<ModeToggle latest={null} />);
  expect((screen.getByRole("button") as HTMLButtonElement).disabled).toBe(true);
});
