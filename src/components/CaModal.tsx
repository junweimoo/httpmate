import { useState } from "react";
import { api } from "../ipc";
import { useStore } from "../store";

export function CaModal() {
  const ca = useStore((s) => s.ca);
  const setCa = useStore((s) => s.setCa);
  const setCaModalOpen = useStore((s) => s.setCaModalOpen);
  const setError = useStore((s) => s.setError);
  const [busy, setBusy] = useState(false);
  const [exportedPath, setExportedPath] = useState<string | null>(ca?.certPath ?? null);
  const [copied, setCopied] = useState(false);

  const generateAndExport = async () => {
    setBusy(true);
    try {
      const exported = await api.exportCa();
      setExportedPath(exported.path);
      setCa(await api.caState());
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const copyPem = async () => {
    try {
      const exported = await api.exportCa();
      await navigator.clipboard.writeText(exported.pem);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch (e) {
      setError(String(e));
    }
  };

  const installTrust = async () => {
    setBusy(true);
    try {
      setCa(await api.installCaTrust());
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const trustLabel =
    ca?.trusted === true ? "trusted ✓" : ca?.trusted === false ? "NOT trusted" : "trust unknown";

  return (
    <div className="modal-backdrop" onClick={() => setCaModalOpen(false)}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>HTTPS interception certificate</h2>
        <p>
          To show decrypted HTTPS traffic, httpmate generates a local root
          certificate and mints per-site certificates on the fly. The root must
          be trusted by this Mac (or by the device/simulator you are
          debugging). Its private key never leaves your keychain.
        </p>
        <div className="ca-status">
          <div>
            CA generated: <b>{ca?.generated ? "yes" : "not yet"}</b>
          </div>
          <div>
            System trust: <b>{ca?.generated ? trustLabel : "—"}</b>
          </div>
          {exportedPath && (
            <div className="ca-path">
              certificate file: <code>{exportedPath}</code>
            </div>
          )}
        </div>
        <div className="modal-actions">
          {!ca?.generated && (
            <button className="btn start" disabled={busy} onClick={() => void generateAndExport()}>
              Generate CA
            </button>
          )}
          {ca?.generated && (
            <>
              <button className="btn start" disabled={busy} onClick={() => void installTrust()}>
                Install trust (macOS)
              </button>
              <button className="btn" disabled={busy} onClick={() => void generateAndExport()}>
                Show cert path
              </button>
              <button className="btn" disabled={busy} onClick={() => void copyPem()}>
                {copied ? "Copied!" : "Copy PEM"}
              </button>
            </>
          )}
          <span className="spacer" />
          <button className="btn" onClick={() => setCaModalOpen(false)}>
            Close
          </button>
        </div>
        <p className="modal-hint">
          Hosts that pin their certificates (many native apps) cannot be
          decrypted; httpmate detects this and tunnels them opaquely. You can
          also add passthrough hosts in settings.json.
        </p>
      </div>
    </div>
  );
}
