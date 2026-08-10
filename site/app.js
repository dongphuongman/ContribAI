document.documentElement.classList.add("js");

const status = document.querySelector(".copy-status");
let statusTimer;

function announce(message) {
  if (!status) return;

  window.clearTimeout(statusTimer);
  status.textContent = message;
  status.classList.add("is-visible");
  statusTimer = window.setTimeout(() => {
    status.classList.remove("is-visible");
  }, 2200);
}

function copyWithSelection(text) {
  const input = document.createElement("textarea");
  input.value = text;
  input.setAttribute("readonly", "");
  input.style.position = "fixed";
  input.style.opacity = "0";
  document.body.append(input);
  input.select();

  try {
    return document.execCommand("copy");
  } finally {
    input.remove();
  }
}

async function copyText(text) {
  if (navigator.clipboard && window.isSecureContext) {
    await navigator.clipboard.writeText(text);
    return true;
  }

  return copyWithSelection(text);
}

for (const button of document.querySelectorAll("[data-copy]")) {
  button.addEventListener("click", async () => {
    const target = document.getElementById(button.dataset.copy);
    if (!target) {
      announce("Command unavailable");
      return;
    }

    const originalLabel = button.textContent;

    try {
      const copied = await copyText(target.textContent.trim());
      if (!copied) throw new Error("Copy command was rejected");

      button.textContent = "Copied";
      announce("Command copied to clipboard");
    } catch {
      announce("Select the command and copy it manually");
    } finally {
      window.setTimeout(() => {
        button.textContent = originalLabel;
      }, 1600);
    }
  });
}

const demoButton = document.querySelector("[data-demo-start]");
const demoLines = [...document.querySelectorAll("[data-demo-line]")];
const demoLive = document.querySelector(".demo-live");
let demoTimers = [];

function clearDemoTimers() {
  for (const timer of demoTimers) window.clearTimeout(timer);
  demoTimers = [];
}

function runDemoWalkthrough() {
  if (!demoButton || demoLines.length === 0) return;

  clearDemoTimers();
  demoButton.disabled = true;
  demoButton.textContent = "Running policy…";
  for (const line of demoLines) line.classList.remove("is-active");

  const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const interval = reducedMotion ? 0 : 520;

  demoLines.forEach((line, index) => {
    demoTimers.push(
      window.setTimeout(() => {
        line.classList.add("is-active");
        if (demoLive) {
          const label = line.querySelector("strong")?.textContent ?? "Policy step";
          const outcome = line.querySelector("b")?.textContent ?? "complete";
          demoLive.textContent = label + ": " + outcome;
        }
      }, interval * index),
    );
  });

  demoTimers.push(
    window.setTimeout(
      () => {
        demoButton.disabled = false;
        demoButton.textContent = "Run again";
        if (demoLive) {
          demoLive.textContent =
            "Walkthrough complete. External writes remained disabled and submission was not attempted.";
        }
      },
      interval * demoLines.length + (reducedMotion ? 0 : 180),
    ),
  );
}

if (demoButton) demoButton.addEventListener("click", runDemoWalkthrough);
