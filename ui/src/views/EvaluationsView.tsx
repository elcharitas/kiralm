import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Plus, Trash2 } from "lucide-react";
import {
  api,
  type EvalAssertion,
  type EvalAssertionTarget,
  type EvalRun,
  type EvalSuite,
  type StatusResponse,
} from "../api";
import { useDialogs } from "../components/Dialogs";
import { Select } from "../components/Select";
import {
  positiveInteger,
  unusedNumberedId,
  validateAssertionValue,
} from "../evaluationValidation";
import { modelName } from "../format";

type AssertionKind = EvalAssertion["type"];

interface AssertionDraft {
  type: AssertionKind;
  target: EvalAssertionTarget;
  value: string;
}

interface MetadataDraft {
  key: string;
  value: string;
}

interface CaseDraft {
  id: string;
  prompt: string;
  model: string;
  temperature: string;
  maxTokens: string;
  seed: string;
  timeoutMs: string;
  assertions: AssertionDraft[];
  metadata: MetadataDraft[];
}

interface SuiteDraft {
  id: string;
  name: string;
  description: string;
  defaultModel: string;
  cases: CaseDraft[];
}

interface ActiveEvalRun {
  runId: string;
  suiteId: string;
}

const assertionOptions: Array<{ value: AssertionKind; label: string }> = [
  { value: "exact", label: "Exact match" },
  { value: "contains", label: "Contains text" },
  { value: "regex", label: "Regular expression" },
  { value: "json_equals", label: "JSON equals" },
  { value: "json_shape", label: "JSON shape" },
  { value: "max_latency_ms", label: "Maximum latency" },
  { value: "max_total_tokens", label: "Maximum tokens" },
];

function emptyAssertion(): AssertionDraft {
  return { type: "contains", target: "output", value: "" };
}

function emptyCase(id: string): CaseDraft {
  return {
    id,
    prompt: "",
    model: "",
    temperature: "0",
    maxTokens: "512",
    seed: "0",
    timeoutMs: "",
    assertions: [emptyAssertion()],
    metadata: [],
  };
}

function starterDraft(model: string): SuiteDraft {
  return {
    id: `suite-${Date.now()}`,
    name: "New evaluation suite",
    description: "A deterministic regression suite.",
    defaultModel: model,
    cases: [
      {
        ...emptyCase("case-1"),
        id: "hello",
        prompt: "Reply with exactly: hello",
        maxTokens: "32",
        assertions: [
          { type: "exact", target: "output", value: "hello" },
        ],
      },
    ],
  };
}

function assertionToDraft(assertion: EvalAssertion): AssertionDraft {
  const target = "target" in assertion ? assertion.target : "output";
  if (assertion.type === "regex") {
    return { type: assertion.type, target, value: assertion.pattern };
  }
  if (assertion.type === "json_equals") {
    return {
      type: assertion.type,
      target,
      value: JSON.stringify(assertion.expected, null, 2),
    };
  }
  if (assertion.type === "json_shape") {
    return {
      type: assertion.type,
      target,
      value: JSON.stringify(assertion.shape, null, 2),
    };
  }
  if (
    assertion.type === "max_latency_ms" ||
    assertion.type === "max_total_tokens"
  ) {
    return { type: assertion.type, target, value: String(assertion.max) };
  }
  return { type: assertion.type, target, value: assertion.expected };
}

function suiteToDraft(suite: EvalSuite): SuiteDraft {
  return {
    id: suite.id,
    name: suite.name,
    description: suite.description,
    defaultModel: suite.default_model ?? "",
    cases: suite.cases.map((evalCase) => ({
      id: evalCase.id,
      prompt: evalCase.prompt,
      model: evalCase.model ?? "",
      temperature: String(evalCase.settings?.temperature ?? 0),
      maxTokens: String(evalCase.settings?.max_tokens ?? 512),
      seed: evalCase.settings?.seed == null
        ? ""
        : String(evalCase.settings.seed),
      timeoutMs: evalCase.timeout_ms == null
        ? ""
        : String(evalCase.timeout_ms),
      assertions: evalCase.assertions.map(assertionToDraft),
      metadata: Object.entries(evalCase.metadata ?? {}).map(
        ([key, value]) => ({ key, value }),
      ),
    })),
  };
}

function assertionFromDraft(draft: AssertionDraft): EvalAssertion {
  validateAssertionValue(draft.type, draft.value);
  if (draft.type === "regex") {
    return { type: draft.type, target: draft.target, pattern: draft.value };
  }
  if (draft.type === "json_equals") {
    return {
      type: draft.type,
      target: draft.target,
      expected: JSON.parse(draft.value),
    };
  }
  if (draft.type === "json_shape") {
    return {
      type: draft.type,
      target: draft.target,
      shape: JSON.parse(draft.value),
    };
  }
  if (draft.type === "max_latency_ms") {
    return {
      type: draft.type,
      max: positiveInteger(draft.value, "Maximum latency"),
    };
  }
  if (draft.type === "max_total_tokens") {
    return {
      type: draft.type,
      max: positiveInteger(draft.value, "Maximum tokens"),
    };
  }
  return {
    type: draft.type,
    target: draft.target,
    expected: draft.value,
  };
}

function suiteFromDraft(draft: SuiteDraft): EvalSuite {
  if (!draft.id.trim() || !draft.name.trim()) {
    throw new Error("Suite id and name are required");
  }
  if (draft.cases.length === 0) {
    throw new Error("Add at least one evaluation case");
  }
  const caseIds = new Set<string>();
  return {
    id: draft.id.trim(),
    name: draft.name.trim(),
    description: draft.description.trim(),
    default_model: draft.defaultModel || null,
    cases: draft.cases.map((evalCase) => {
      if (!evalCase.id.trim() || !evalCase.prompt.trim()) {
        throw new Error("Every case needs an id and prompt");
      }
      if (caseIds.has(evalCase.id.trim())) {
        throw new Error(`Case id ${evalCase.id.trim()} is duplicated`);
      }
      caseIds.add(evalCase.id.trim());
      if (evalCase.assertions.length === 0) {
        throw new Error(`Case ${evalCase.id} needs at least one assertion`);
      }
      const temperature = Number(evalCase.temperature);
      if (
        !Number.isFinite(temperature) ||
        temperature < 0 ||
        temperature > 2
      ) {
        throw new Error(`Case ${evalCase.id} temperature must be 0 to 2`);
      }
      const seed = evalCase.seed === "" ? null : Number(evalCase.seed);
      if (seed != null && (!Number.isSafeInteger(seed) || seed < 0)) {
        throw new Error(`Case ${evalCase.id} has an invalid seed`);
      }
      return {
        id: evalCase.id.trim(),
        prompt: evalCase.prompt.trim(),
        model: evalCase.model || null,
        settings: {
          temperature,
          max_tokens: positiveInteger(
            evalCase.maxTokens,
            `Case ${evalCase.id} max tokens`,
            4_294_967_295,
          ),
          seed,
        },
        timeout_ms: evalCase.timeoutMs === ""
          ? null
          : positiveInteger(
              evalCase.timeoutMs,
              `Case ${evalCase.id} timeout`,
              600_000,
            ),
        assertions: evalCase.assertions.map(assertionFromDraft),
        metadata: Object.fromEntries(
          evalCase.metadata
            .filter((entry) => entry.key.trim())
            .map((entry) => [entry.key.trim(), entry.value]),
        ),
      };
    }),
  };
}

export function EvaluationsView({ status }: { status: StatusResponse | null }) {
  const { toast } = useDialogs();
  const firstModelId = status?.models[0]?.id ?? "";
  const firstModelIdRef = useRef(firstModelId);
  const [suites, setSuites] = useState<EvalSuite[]>([]);
  const [selectedId, setSelectedId] = useState("");
  const [draft, setDraft] = useState<SuiteDraft | null>(null);
  const [runs, setRuns] = useState<EvalRun[]>([]);
  const [baselineId, setBaselineId] = useState("");
  const [activeRun, setActiveRun] = useState<ActiveEvalRun | null>(null);
  const [loading, setLoading] = useState(true);

  const modelOptions = [
    { value: "", label: "Use suite default" },
    ...(status?.models ?? []).map((model) => ({
      value: model.id,
      label: modelName(model.id),
    })),
  ];

  const loadSuites = useCallback(async () => {
    const result = await api.listEvalSuites();
    setSuites(result.suites);
    return result.suites;
  }, []);

  useEffect(() => {
    if (firstModelId) firstModelIdRef.current = firstModelId;
  }, [firstModelId]);

  useEffect(() => {
    void loadSuites()
      .then((loaded) => {
        if (loaded.length > 0) {
          setSelectedId(loaded[0].id);
        } else {
          setDraft(starterDraft(firstModelIdRef.current));
        }
      })
      .catch((error) =>
        toast(error instanceof Error ? error.message : String(error), "error"),
      )
      .finally(() => setLoading(false));
  }, [loadSuites, toast]);

  useEffect(() => {
    const suite = suites.find((item) => item.id === selectedId);
    if (suite) setDraft(suiteToDraft(suite));
    setBaselineId("");
    if (!selectedId) {
      setRuns([]);
      return;
    }
    let disposed = false;
    void api.listEvalRuns(selectedId).then(
      (result) => {
        if (!disposed) setRuns(result.runs);
      },
      (error) => {
        if (!disposed) {
          toast(error instanceof Error ? error.message : String(error), "error");
        }
      },
    );
    return () => {
      disposed = true;
    };
  }, [selectedId, suites, toast]);

  useEffect(() => {
    if (!activeRun) return;
    const { runId, suiteId } = activeRun;
    let disposed = false;
    const timer = window.setInterval(() => {
      void api
        .evalJob(runId)
        .then(async (job) => {
          if (disposed || job.running) return;
          window.clearInterval(timer);
          const result = await api.listEvalRuns(suiteId);
          if (disposed) return;
          if (selectedId === suiteId) setRuns(result.runs);
          if (result.runs.some((run) => run.id === runId)) {
            toast("Evaluation completed", "success");
          } else {
            toast("Evaluation failed before producing a result", "error");
          }
          setActiveRun((current) =>
            current?.runId === runId ? null : current,
          );
        })
        .catch((error) => {
          if (!disposed) {
            window.clearInterval(timer);
            setActiveRun((current) =>
              current?.runId === runId ? null : current,
            );
            toast(error instanceof Error ? error.message : String(error), "error");
          }
        });
    }, 750);
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, [activeRun, selectedId, toast]);

  const latestRun = runs[0] ?? null;
  const selectedSuite = useMemo(
    () => suites.find((suite) => suite.id === selectedId) ?? null,
    [selectedId, suites],
  );

  const saveSuite = async () => {
    if (!draft) return;
    try {
      const saved = await api.saveEvalSuite(suiteFromDraft(draft));
      const loaded = await loadSuites();
      setSelectedId(saved.id);
      setDraft(suiteToDraft(saved));
      if (!loaded.some((suite) => suite.id === saved.id)) {
        setSuites([...loaded, saved]);
      }
      toast("Evaluation suite saved", "success");
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  };

  const createSuite = () => {
    setSelectedId("");
    setRuns([]);
    setDraft(starterDraft(firstModelId));
  };

  const updateCase = (index: number, patch: Partial<CaseDraft>) => {
    setDraft((current) => current && ({
      ...current,
      cases: current.cases.map((evalCase, caseIndex) =>
        caseIndex === index ? { ...evalCase, ...patch } : evalCase,
      ),
    }));
  };

  const updateAssertion = (
    caseIndex: number,
    assertionIndex: number,
    patch: Partial<AssertionDraft>,
  ) => {
    if (!draft) return;
    updateCase(caseIndex, {
      assertions: draft.cases[caseIndex].assertions.map(
        (assertion, index) => index === assertionIndex
          ? { ...assertion, ...patch }
          : assertion,
      ),
    });
  };

  const runSuite = async () => {
    if (!selectedSuite) return;
    try {
      const started = await api.startEvalRun(
        selectedSuite.id,
        baselineId || null,
      );
      setActiveRun({ runId: started.run_id, suiteId: selectedSuite.id });
      toast("Evaluation started", "success");
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  };

  const cancelRun = async () => {
    if (!activeRun) return;
    try {
      await api.cancelEvalRun(activeRun.runId);
      toast("Cancellation requested", "success");
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  };

  const downloadRun = async (run: EvalRun) => {
    try {
      const body = await api.exportEvalRun(run.suite_id, run.id, "json");
      const url = URL.createObjectURL(
        new Blob([body], { type: "application/json" }),
      );
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
              Build deterministic prompts and regression checks.
            </p>
          </div>
          <button
            type="button"
            className="btn"
            disabled={activeRun !== null}
            onClick={createSuite}
          >
            <Plus size={13} /> New suite
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
              disabled={activeRun !== null}
              onChange={setSelectedId}
              options={suites.map((suite) => ({
                value: suite.id,
                label: suite.name,
              }))}
            />
          </div>
        ) : (
          <p className="settings-hint">Create your first suite below.</p>
        )}

        {draft && (
          <>
            <div className="eval-form-grid">
              <label className="settings-field">
                <span>Suite id</span>
                <input
                  className="settings-input mono"
                  value={draft.id}
                  onChange={(event) => setDraft({
                    ...draft,
                    id: event.target.value,
                  })}
                />
              </label>
              <label className="settings-field">
                <span>Name</span>
                <input
                  className="settings-input"
                  value={draft.name}
                  onChange={(event) => setDraft({
                    ...draft,
                    name: event.target.value,
                  })}
                />
              </label>
              <label className="settings-field eval-form-wide">
                <span>Description</span>
                <input
                  className="settings-input"
                  value={draft.description}
                  onChange={(event) => setDraft({
                    ...draft,
                    description: event.target.value,
                  })}
                />
              </label>
              <div className="settings-field eval-form-wide">
                <label>Default model</label>
                <Select
                  triggerClassName="settings-select"
                  value={draft.defaultModel}
                  onChange={(defaultModel) => setDraft({
                    ...draft,
                    defaultModel,
                  })}
                  options={modelOptions.map((option, index) => ({
                    ...option,
                    label: index === 0 ? "No default model" : option.label,
                  }))}
                />
              </div>
            </div>

            <div className="eval-case-list">
              {draft.cases.map((evalCase, caseIndex) => (
                <article className="eval-case-card" key={caseIndex}>
                  <div className="eval-case-heading">
                    <strong>Case {caseIndex + 1}</strong>
                    <button
                      type="button"
                      className="right-panel-icon-btn"
                      title="Remove case"
                      onClick={() => setDraft({
                        ...draft,
                        cases: draft.cases.filter(
                          (_, index) => index !== caseIndex,
                        ),
                      })}
                    >
                      <Trash2 size={13} />
                    </button>
                  </div>
                  <div className="eval-form-grid">
                    <label className="settings-field">
                      <span>Case id</span>
                      <input
                        className="settings-input mono"
                        value={evalCase.id}
                        onChange={(event) => updateCase(caseIndex, {
                          id: event.target.value,
                        })}
                      />
                    </label>
                    <div className="settings-field">
                      <label>Model</label>
                      <Select
                        triggerClassName="settings-select"
                        value={evalCase.model}
                        onChange={(model) => updateCase(caseIndex, { model })}
                        options={modelOptions}
                      />
                    </div>
                    <label className="settings-field eval-form-wide">
                      <span>Prompt</span>
                      <textarea
                        className="settings-input eval-prompt"
                        value={evalCase.prompt}
                        onChange={(event) => updateCase(caseIndex, {
                          prompt: event.target.value,
                        })}
                      />
                    </label>
                    <label className="settings-field">
                      <span>Temperature</span>
                      <input
                        className="settings-input"
                        type="number"
                        min="0"
                        max="2"
                        step="0.1"
                        value={evalCase.temperature}
                        onChange={(event) => updateCase(caseIndex, {
                          temperature: event.target.value,
                        })}
                      />
                    </label>
                    <label className="settings-field">
                      <span>Maximum tokens</span>
                      <input
                        className="settings-input"
                        type="number"
                        min="1"
                        value={evalCase.maxTokens}
                        onChange={(event) => updateCase(caseIndex, {
                          maxTokens: event.target.value,
                        })}
                      />
                    </label>
                    <label className="settings-field">
                      <span>Seed</span>
                      <input
                        className="settings-input"
                        type="number"
                        min="0"
                        placeholder="Random"
                        value={evalCase.seed}
                        onChange={(event) => updateCase(caseIndex, {
                          seed: event.target.value,
                        })}
                      />
                    </label>
                    <label className="settings-field">
                      <span>Timeout in milliseconds</span>
                      <input
                        className="settings-input"
                        type="number"
                        min="1"
                        max="600000"
                        placeholder="120000"
                        value={evalCase.timeoutMs}
                        onChange={(event) => updateCase(caseIndex, {
                          timeoutMs: event.target.value,
                        })}
                      />
                    </label>
                  </div>

                  <div className="eval-subheading">
                    <strong>Assertions</strong>
                    <button
                      type="button"
                      className="btn"
                      onClick={() => updateCase(caseIndex, {
                        assertions: [
                          ...evalCase.assertions,
                          emptyAssertion(),
                        ],
                      })}
                    >
                      <Plus size={12} /> Add assertion
                    </button>
                  </div>
                  <div className="eval-assertion-list">
                    {evalCase.assertions.map((assertion, assertionIndex) => {
                      const hasTarget = !assertion.type.startsWith("max_");
                      const isJson = assertion.type.startsWith("json_");
                      return (
                        <div className="eval-assertion-row" key={assertionIndex}>
                          <Select
                            triggerClassName="settings-select"
                            value={assertion.type}
                            onChange={(type) => updateAssertion(
                              caseIndex,
                              assertionIndex,
                              { type, value: "" },
                            )}
                            options={assertionOptions}
                          />
                          {hasTarget && (
                            <Select
                              triggerClassName="settings-select"
                              value={assertion.target}
                              onChange={(target) => updateAssertion(
                                caseIndex,
                                assertionIndex,
                                { target },
                              )}
                              options={[
                                { value: "output", label: "Output" },
                                ...(assertion.target === "tool_trace"
                                  ? [{
                                      value: "tool_trace" as const,
                                      label: "Tool trace (existing)",
                                    }]
                                  : []),
                              ]}
                            />
                          )}
                          {!hasTarget && (
                            <span
                              className="eval-assertion-target-placeholder"
                              aria-hidden="true"
                            />
                          )}
                          {isJson ? (
                            <textarea
                              className="settings-input mono eval-assertion-value"
                              placeholder="Expected JSON value"
                              value={assertion.value}
                              onChange={(event) => updateAssertion(
                                caseIndex,
                                assertionIndex,
                                { value: event.target.value },
                              )}
                            />
                          ) : (
                            <input
                              className="settings-input"
                              type={hasTarget ? "text" : "number"}
                              min={hasTarget ? undefined : "1"}
                              placeholder={hasTarget ? "Expected value" : "Maximum"}
                              value={assertion.value}
                              onChange={(event) => updateAssertion(
                                caseIndex,
                                assertionIndex,
                                { value: event.target.value },
                              )}
                            />
                          )}
                          <button
                            type="button"
                            className="right-panel-icon-btn"
                            title="Remove assertion"
                            onClick={() => updateCase(caseIndex, {
                              assertions: evalCase.assertions.filter(
                                (_, index) => index !== assertionIndex,
                              ),
                            })}
                          >
                            <Trash2 size={12} />
                          </button>
                        </div>
                      );
                    })}
                  </div>

                  <div className="eval-subheading">
                    <strong>Metadata</strong>
                    <button
                      type="button"
                      className="btn"
                      onClick={() => updateCase(caseIndex, {
                        metadata: [
                          ...evalCase.metadata,
                          { key: "", value: "" },
                        ],
                      })}
                    >
                      <Plus size={12} /> Add field
                    </button>
                  </div>
                  {evalCase.metadata.map((entry, metadataIndex) => (
                    <div className="eval-metadata-row" key={metadataIndex}>
                      <input
                        className="settings-input mono"
                        placeholder="Key"
                        value={entry.key}
                        onChange={(event) => updateCase(caseIndex, {
                          metadata: evalCase.metadata.map((item, index) =>
                            index === metadataIndex
                              ? { ...item, key: event.target.value }
                              : item,
                          ),
                        })}
                      />
                      <input
                        className="settings-input"
                        placeholder="Value"
                        value={entry.value}
                        onChange={(event) => updateCase(caseIndex, {
                          metadata: evalCase.metadata.map((item, index) =>
                            index === metadataIndex
                              ? { ...item, value: event.target.value }
                              : item,
                          ),
                        })}
                      />
                      <button
                        type="button"
                        className="right-panel-icon-btn"
                        title="Remove metadata field"
                        onClick={() => updateCase(caseIndex, {
                          metadata: evalCase.metadata.filter(
                            (_, index) => index !== metadataIndex,
                          ),
                        })}
                      >
                        <Trash2 size={12} />
                      </button>
                    </div>
                  ))}
                </article>
              ))}
            </div>

            <div className="settings-actions eval-suite-actions">
              <button
                type="button"
                className="btn"
                onClick={() => setDraft({
                  ...draft,
                  cases: [
                    ...draft.cases,
                    emptyCase(unusedNumberedId(
                      draft.cases.map((evalCase) => evalCase.id),
                      "case",
                    )),
                  ],
                })}
              >
                <Plus size={13} /> Add case
              </button>
              <button
                type="button"
                className="btn btn-accent"
                onClick={() => void saveSuite()}
              >
                Save suite
              </button>
            </div>
          </>
        )}
      </section>

      {selectedSuite && (
        <section className="settings-section">
          <h3>Run and compare</h3>
          <p className="settings-hint">
            Runs use bounded concurrency, deterministic seeds, and timeouts.
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
              {activeRun?.suiteId === selectedSuite.id
                ? "Running..."
                : activeRun
                  ? "Another suite is running"
                  : "Run suite"}
            </button>
            {activeRun && (
              <button type="button" className="btn" onClick={() => void cancelRun()}>
                Cancel run
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
              <p className="settings-hint">
                {new Date(latestRun.finished_at).toLocaleString()}
              </p>
            </div>
            <button
              type="button"
              className="btn"
              onClick={() => void downloadRun(latestRun)}
            >
              Export results
            </button>
          </div>
          <div className="eval-metrics">
            <span>{Math.round(latestRun.metrics.pass_rate * 100)}% passed</span>
            <span>{Math.round(latestRun.metrics.mean_latency_ms)} ms mean</span>
            <span>{latestRun.metrics.p95_latency_ms} ms p95</span>
            <span>{latestRun.metrics.total_tokens} tokens</span>
          </div>
          {latestRun.baseline && (
            <p className={latestRun.baseline.regressions.length
              ? "eval-regressed"
              : "eval-passed"}
            >
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
                <span className={`eval-status eval-status-${result.status}`}>
                  {result.status}
                </span>
              </div>
            ))}
          </div>
        </section>
      )}
    </div>
  );
}
