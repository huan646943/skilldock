import { expect, test, vi } from "vitest";

vi.mock("@/app/is-tauri-runtime", () => ({
  isTauriRuntime: () => false,
}));

const { recordFailureFeedback } = await import("@/features/skills/api/skill-client");

test("includes root cause details in fallback feedback draft", async () => {
  const draft = await recordFailureFeedback({
    operation: "check_for_app_update",
    message: "error sending request for url (https://github.com/example/latest.json)",
    context: {
      errorDetails: {
        rootCause: "dns error: failed to lookup address information",
        causeChain: [
          "client error (Connect)",
          "dns error: failed to lookup address information",
        ],
      },
    },
  });

  const issueUrl = new URL(draft.issueUrl);
  expect(issueUrl.origin).toBe("https://github.com");
  expect(issueUrl.pathname).toBe("/huan646943/skilldock/issues/new");
  expect(issueUrl.searchParams.get("title")).toContain("check_for_app_update");
  expect(issueUrl.searchParams.get("body")).toContain("dns error");
  expect(draft.body).toContain("rootCause: dns error: failed to lookup address information");
  expect(draft.body).toContain(
    "causeChain: client error (Connect) -> dns error: failed to lookup address information",
  );
});
