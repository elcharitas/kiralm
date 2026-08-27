import { useCallback, useEffect, useState } from "react";
import { api, type Project } from "../api";

// Owns the registered-project list, backed by the daemon's persisted
// /api/projects. Loaded once on mount; `refresh` re-fetches after any
// mutating action (create/delete/touch) done elsewhere.
export function useProjects() {
  const [projects, setProjects] = useState<Project[]>([]);

  const refresh = useCallback(async () => {
    const { projects } = await api.listProjects();
    setProjects(projects);
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const createProject = async (path: string, name?: string) => {
    const project = await api.createProject(path, name);
    await refresh();
    return project;
  };

  const cloneProject = async (url: string, path: string, name?: string) => {
    const project = await api.cloneProject(url, path, name);
    await refresh();
    return project;
  };

  const deleteProject = async (id: string) => {
    await api.deleteProject(id);
    await refresh();
  };

  return { projects, refresh, createProject, cloneProject, deleteProject };
}
