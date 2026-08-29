import { useCallback, useEffect, useMemo, useState } from "react";
import {
  api,
  type EvalRun,
  type EvalSuite,
  type StatusResponse,
} from "../api";
import { useDialogs } from "../components/Dialogs";
import { Select } from "../components/Select";

function starterSuite(model: string): EvalSuite {
  return {
    id: `suite-${Date.now()}`,
    name: "New evaluation suite",
    description: "A deterministic regression suite.",
    default_model: model || null,
    cases: [
      {
        id: "hello",
        prompt: "Reply with exactly: hello",
        settings: { temperature: 0, max_tokens: 32, seed: 0 },
        assertions: [{ type: "exact", target: "output", expected: "hello" }],
        metadata: {},
      },
    ],
  };
}

export function EvaluationsView({ status }: { status: StatusResponse | null }) {
  const { toast } = useDialogs();
  const [suites, setSuites] = useState<EvalSuite[]>([]);
  const [selectedId, setSelectedId] = useState("");
  const [editor, setEditor] = useState("");
  const [runs, setRuns] = useState<EvalRun[]>([]);
  const [baselineId, setBaselineId] = useState("");
  const [activeRun, setActiveRun] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const loadSuites = useCallback(async () => {
    const result = await api.listEvalSuites();
    setSuites(result.suites);
    return result.suites;
  }, []);

  const loadRuns = useCallback(async (suiteId: string) => {
    if (!suiteId) {
      setRuns([]);
      return [];
    }
    const result = await api.listEvalRuns(suiteId);
    setRuns(result.runs);
    return result.runs;
  }, []);

  useEffect(() => {
    void loadSuites()
      .then((loaded) => {
        if (loaded.length > 0) setSelectedId(loaded[0].id);
      })
      .catch((error) =>
        toast(error instanceof Error ? error.message : String(error), "error"),
      )
      .finally(() => setLoading(false));
  }, [loadSuites, toast]);

  useEffect(() => {
    const suite = suites.find((item) => item.id === selectedId);
    if (suite) setEditor(JSON.stringify(suite, null, 2));
    setBaselineId("");
    void loadRuns(selectedId).catch((error) =>
      toast(error instanceof Error ? error.message : String(error), "error"),
    );
  }, [loadRuns, selectedId, suites, toast]);

  useEffect(() => {
    if (!activeRun || !selectedId) return;
    let disposed = false;
    const timer = window.setInterval(() => {
      void api
        .evalJob(activeRun)
        .then(async (job) => {
          if (disposed || job.running) return;
          window.clearInterval(timer);
          const latest = await loadRuns(selectedId);
          if (disposed) return;
          if (latest.some((run) => run.id === activeRun)) {
            toast("Evaluation completed", "success");
          } else {
            toast("Evaluation failed before producing a result", "error");
          }
          setActiveRun(null);
        })
        .catch((error) => {
          if (!disposed) {
            window.clearInterval(timer);
            setActiveRun(null);
            toast(error instanceof Error ? error.message : String(error), "error");
          }
        });
    }, 750);
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, [activeRun, loadRuns, selectedId, toast]);

  const latestRun = runs[0] ?? null;
  const selectedSuite = useMemo(
    () => suites.find((suite) => suite.id === selectedId) ?? null,
    [selectedId, suites],
  );

  const saveSuite = async () => {
    try {
      const parsed = JSON.parse(editor) as EvalSuite;
      const saved = await api.saveEvalSuite(parsed);
      const loaded = await loadSuites();
      setSelectedId(saved.id);
      setEditor(JSON.stringify(saved, null, 2));
      if (!loaded.some((suite) => suite.id === saved.id)) {
        setSuites([...loaded, saved]);
      }
      toast("Evaluation suite saved", "success");
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  };

  const createSuite = () => {
    const suite = starterSuite(status?.models[0]?.id ?? "");
    setSelectedId("");
    setRuns([]);
    setEditor(JSON.stringify(suite, null, 2));
  };

  const runSuite = async () => {
    if (!selectedSuite) return;
    try {
      const started = await api.startEvalRun(selectedSuite.id, baselineId || null);
      setActiveRun(started.run_id);
      toast("Evaluation started", "success");
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  };

  const cancelRun = async () => {
    if (!activeRun) return;
    try {
      await api.cancelEvalRun(activeRun);
      toast("Cancellation requested", "success");
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  };

  const downloadRun = async (run: EvalRun) => {
    try {
      const body = await api.exportEvalRun(run.suite_id, run.id, "json");
      const url = URL.createObjectURL(new Blob([body], { type: "application/json" }));
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = `${run.suite_id}-${run.id}.json`;
      anchor.click();
      URL.revokeObjectURL(url);
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  };

  return (
    <div className="settings-scroll eval-view">
      <section className="settings-section">
        <div className="eval-heading">
          <div>
            <h3>Evaluation suites</h3>
            <p className="settings-hint">
              Run deterministic prompts, assertions, and regression checks against local or cloud models.
            </p>
          </div>
          <button type="button" className="btn" onClick={createSuite}>
            New suite
          </button>
        </div>
        {loading ? (
          <p className="settings-hint">Loading suites...</p>
        ) : suites.length > 0 ? (
          <div className="settings-field">
            <label>Suite</label>
            <Select
              triggerClassName="settings-select"
              value={selectedId}
              onChange={setSelectedId}
              options={suites.map((suite) => ({ value: suite.id, label: suite.name }))}
            />
          </div>
        ) : (
          <p className="settings-hint">No suites yet. Create one to get started.</p>
        )}
        <div className="settings-field">
          <label htmlFor="eval-suite-json">Suite JSON</label>
          <textarea
            id="eval-suite-json"
            className="settings-input mono eval-editor"
            spellCheck={false}
            value={editor}
            onChange={(event) => setEditor(event.target.value)}
          />
        </div>
        <div className="settings-actions">
          <button type="button" className="btn btn-accent" onClick={() => void saveSuite()}>
            Save suite
          </button>
        </div>
      </section>

      {selectedSuite && (
        <section className="settings-section">
          <h3>Run and compare</h3>
          <p className="settings-hint">
            Runs use two concurrent cases, deterministic seeds, and a two-minute case timeout.
          </p>
          <div className="settings-field">
            <label>Baseline</label>
            <Select
              triggerClassName="settings-select"
              value={baselineId}
              onChange={setBaselineId}
              options={[
                { value: "", label: "No baseline" },
                ...runs.map((run) => ({
                  value: run.id,
                  label: `${new Date(run.finished_at).toLocaleString()} · ${Math.round(run.metrics.pass_rate * 100)}%`,
                })),
              ]}
            />
          </div>
          <div className="settings-actions">
            <button
              type="button"
              className="btn btn-accent"
              disabled={activeRun !== null}
              onClick={() => void runSuite()}
            >
              {activeRun ? "Running..." : "Run suite"}
            </button>
            {activeRun && (
              <button type="button" className="btn" onClick={() => void cancelRun()}>
                Cancel
              </button>
            )}
          </div>
        </section>
      )}

      {latestRun && (
        <section className="settings-section">
          <div className="eval-heading">
            <div>
              <h3>Latest result</h3>
              <p className="settings-hint">{new Date(latestRun.finished_at).toLocaleString()}</p>
            </div>
            <button type="button" className="btn" onClick={() => void downloadRun(latestRun)}>
              Export JSON
            </button>
          </div>
          <div className="eval-metrics">
            <span>{Math.round(latestRun.metrics.pass_rate * 100)}% passed</span>
            <span>{Math.round(latestRun.metrics.mean_latency_ms)} ms mean</span>
            <span>{latestRun.metrics.p95_latency_ms} ms p95</span>
            <span>{latestRun.metrics.total_tokens} tokens</span>
          </div>
          {latestRun.baseline && (
            <p className={latestRun.baseline.regressions.length ? "eval-regressed" : "eval-passed"}>
              {latestRun.baseline.regressions.length
                ? `${latestRun.baseline.regressions.length} regression threshold(s) exceeded`
                : "No regressions against the selected baseline"}
            </p>
          )}
          <div className="eval-results">
            {latestRun.results.map((result) => (
              <div className="preset-row" key={result.case_id}>
                <div>
                  <strong>{result.case_id}</strong>
                  <span>{result.model} · {result.latency_ms} ms</span>
                </div>
                <span className={`eval-status eval-status-${result.status}`}>{result.status}</span>
              </div>
            ))}
          </div>
        </section>
      )}
    </div>
  );
}
