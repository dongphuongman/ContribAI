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
