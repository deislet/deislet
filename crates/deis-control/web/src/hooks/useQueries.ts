/**
 * React Query Hooks for Control API
 *
 * 提供统一的数据获取和缓存管理
 */

import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { api } from "../lib/api";
import type { ExecuteCommandRequest } from "../lib/api";
import { toast } from "sonner";

// ==================== Workers ====================

export function useWorkers() {
  return useQuery({
    queryKey: ["workers"],
    queryFn: () => api.getWorkers(),
    refetchInterval: 5000, // 每 5 秒刷新
  });
}

export function useWorker(id: string) {
  return useQuery({
    queryKey: ["workers", id],
    queryFn: () => api.getWorker(id),
    enabled: !!id,
    refetchInterval: 3000,
  });
}

export function useWorkerHeartbeats(id: string, limit = 100) {
  return useQuery({
    queryKey: ["workers", id, "heartbeats"],
    queryFn: () => api.getWorkerHeartbeats(id, limit),
    enabled: !!id,
  });
}

// ==================== Configs ====================

export function useConfigs() {
  return useQuery({
    queryKey: ["configs"],
    queryFn: () => api.getConfigs(),
  });
}

export function useConfigStats() {
  return useQuery({
    queryKey: ["configs", "stats"],
    queryFn: () => api.getConfigStats(),
  });
}

export function useWorkerConfig(workerId: string) {
  return useQuery({
    queryKey: ["configs", workerId],
    queryFn: () => api.getWorkerConfig(workerId),
    enabled: !!workerId,
  });
}

export function useUpdateWorkerConfig() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({ workerId, configs }: { workerId: string; configs: any }) =>
      api.updateWorkerConfig(workerId, configs),
    onSuccess: (_, variables) => {
      toast.success("配置已更新");
      queryClient.invalidateQueries({ queryKey: ["configs"] });
      queryClient.invalidateQueries({ queryKey: ["configs", variables.workerId] });
    },
    onError: (error: any) => {
      toast.error(`配置更新失败: ${error.message}`);
    },
  });
}

export function useDeleteWorkerConfig() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (workerId: string) => api.deleteWorkerConfig(workerId),
    onSuccess: () => {
      toast.success("配置已删除");
      queryClient.invalidateQueries({ queryKey: ["configs"] });
    },
    onError: (error: any) => {
      toast.error(`删除失败: ${error.message}`);
    },
  });
}

export function useDeleteConfigKey() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({ workerId, key }: { workerId: string; key: string }) =>
      api.deleteConfigKey(workerId, key),
    onSuccess: (_, variables) => {
      toast.success("配置项已删除");
      queryClient.invalidateQueries({ queryKey: ["configs"] });
      queryClient.invalidateQueries({ queryKey: ["configs", variables.workerId] });
    },
    onError: (error: any) => {
      toast.error(`删除失败: ${error.message}`);
    },
  });
}

export function useConfigHistory(workerId: string, limit = 100) {
  return useQuery({
    queryKey: ["configs", workerId, "history"],
    queryFn: () => api.getConfigHistory(workerId, limit),
    enabled: !!workerId,
  });
}

export function useConfigKeyHistory(workerId: string, key: string, limit = 50) {
  return useQuery({
    queryKey: ["configs", workerId, key, "history"],
    queryFn: () => api.getConfigKeyHistory(workerId, key, limit),
    enabled: !!workerId && !!key,
  });
}

export function useRollbackConfig() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({
      workerId,
      key,
      versionTimestamp,
    }: {
      workerId: string;
      key: string;
      versionTimestamp: string;
    }) => api.rollbackConfig(workerId, key, versionTimestamp),
    onSuccess: (_, variables) => {
      toast.success("配置已回滚");
      queryClient.invalidateQueries({ queryKey: ["configs"] });
      queryClient.invalidateQueries({ queryKey: ["configs", variables.workerId] });
      queryClient.invalidateQueries({ queryKey: ["configs", variables.workerId, "history"] });
    },
    onError: (error: any) => {
      toast.error(`回滚失败: ${error.message}`);
    },
  });
}

export function usePushConfig() {
  return useMutation({
    mutationFn: ({ workerId, key }: { workerId: string; key?: string }) =>
      api.pushConfig(workerId, key),
    onSuccess: () => {
      toast.success("配置已推送到 Worker");
    },
    onError: (error: any) => {
      toast.error(`推送失败: ${error.message}`);
    },
  });
}

// ==================== Commands ====================

export function useCommands() {
  return useQuery({
    queryKey: ["commands"],
    queryFn: () => api.getCommands(),
    refetchInterval: 3000,
  });
}

export function useWorkerCommands(workerId: string) {
  return useQuery({
    queryKey: ["commands", "worker", workerId],
    queryFn: () => api.getWorkerCommands(workerId),
    enabled: !!workerId,
    refetchInterval: 3000,
  });
}

export function useCommandStatus(commandId: string) {
  return useQuery({
    queryKey: ["commands", "status", commandId],
    queryFn: () => api.getCommandStatus(commandId),
    enabled: !!commandId,
    refetchInterval: 1000, // 命令执行时频繁刷新
  });
}

export function useExecuteCommand() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({ workerId, command }: { workerId: string; command: ExecuteCommandRequest }) =>
      api.executeCommand(workerId, command),
    onSuccess: () => {
      toast.success("命令已发送");
      queryClient.invalidateQueries({ queryKey: ["commands"] });
    },
    onError: (error: any) => {
      toast.error(`命令执行失败: ${error.message}`);
    },
  });
}

export function useCancelCommand() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (commandId: string) => api.cancelCommand(commandId),
    onSuccess: () => {
      toast.success("命令已取消");
      queryClient.invalidateQueries({ queryKey: ["commands"] });
    },
    onError: (error: any) => {
      toast.error(`取消失败: ${error.message}`);
    },
  });
}

// ==================== Events ====================

export function useEvents(params?: {
  worker_id?: string;
  severity?: string;
  limit?: number;
  offset?: number;
}) {
  return useQuery({
    queryKey: ["events", params],
    queryFn: () => api.getEvents(params),
    refetchInterval: 5000,
  });
}

export function useEventStats() {
  return useQuery({
    queryKey: ["events", "stats"],
    queryFn: () => api.getEventStats(),
  });
}

// ==================== Metrics ====================

export function useMetricsSummary() {
  return useQuery({
    queryKey: ["metrics", "summary"],
    queryFn: () => api.getMetricsSummary(),
    refetchInterval: 2000, // 每 2 秒刷新
  });
}
