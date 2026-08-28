/**
 * React Query Hooks for Control API
 *
 * 提供统一的数据获取和缓存管理
 */

import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { api } from "../lib/api";
import type { ExecuteCommandRequest } from "../lib/api";
import { toast } from "sonner";

// ==================== Nodes ====================

export function useNodes() {
  return useQuery({
    queryKey: ["nodes"],
    queryFn: () => api.getNodes(),
    refetchInterval: 5000, // 每 5 秒刷新
  });
}

export function useNode(id: string) {
  return useQuery({
    queryKey: ["nodes", id],
    queryFn: () => api.getNode(id),
    enabled: !!id,
    refetchInterval: 3000,
  });
}

export function useNodeHeartbeats(id: string, limit = 100) {
  return useQuery({
    queryKey: ["nodes", id, "heartbeats"],
    queryFn: () => api.getNodeHeartbeats(id, limit),
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

export function useNodeConfig(nodeId: string) {
  return useQuery({
    queryKey: ["configs", nodeId],
    queryFn: () => api.getNodeConfig(nodeId),
    enabled: !!nodeId,
  });
}

export function useUpdateNodeConfig() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({ nodeId, configs }: { nodeId: string; configs: any }) =>
      api.updateNodeConfig(nodeId, configs),
    onSuccess: (_, variables) => {
      toast.success("配置已更新");
      queryClient.invalidateQueries({ queryKey: ["configs"] });
      queryClient.invalidateQueries({ queryKey: ["configs", variables.nodeId] });
    },
    onError: (error: any) => {
      toast.error(`配置更新失败: ${error.message}`);
    },
  });
}

export function useDeleteNodeConfig() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (nodeId: string) => api.deleteNodeConfig(nodeId),
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
    mutationFn: ({ nodeId, key }: { nodeId: string; key: string }) =>
      api.deleteConfigKey(nodeId, key),
    onSuccess: (_, variables) => {
      toast.success("配置项已删除");
      queryClient.invalidateQueries({ queryKey: ["configs"] });
      queryClient.invalidateQueries({ queryKey: ["configs", variables.nodeId] });
    },
    onError: (error: any) => {
      toast.error(`删除失败: ${error.message}`);
    },
  });
}

export function useConfigHistory(nodeId: string, limit = 100) {
  return useQuery({
    queryKey: ["configs", nodeId, "history"],
    queryFn: () => api.getConfigHistory(nodeId, limit),
    enabled: !!nodeId,
  });
}

export function useConfigKeyHistory(nodeId: string, key: string, limit = 50) {
  return useQuery({
    queryKey: ["configs", nodeId, key, "history"],
    queryFn: () => api.getConfigKeyHistory(nodeId, key, limit),
    enabled: !!nodeId && !!key,
  });
}

export function useRollbackConfig() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({
      nodeId,
      key,
      versionTimestamp,
    }: {
      nodeId: string;
      key: string;
      versionTimestamp: string;
    }) => api.rollbackConfig(nodeId, key, versionTimestamp),
    onSuccess: (_, variables) => {
      toast.success("配置已回滚");
      queryClient.invalidateQueries({ queryKey: ["configs"] });
      queryClient.invalidateQueries({ queryKey: ["configs", variables.nodeId] });
      queryClient.invalidateQueries({ queryKey: ["configs", variables.nodeId, "history"] });
    },
    onError: (error: any) => {
      toast.error(`回滚失败: ${error.message}`);
    },
  });
}

export function usePushConfig() {
  return useMutation({
    mutationFn: ({ nodeId, key }: { nodeId: string; key?: string }) =>
      api.pushConfig(nodeId, key),
    onSuccess: () => {
      toast.success("配置已推送到 Node");
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

export function useNodeCommands(nodeId: string) {
  return useQuery({
    queryKey: ["commands", "node", nodeId],
    queryFn: () => api.getNodeCommands(nodeId),
    enabled: !!nodeId,
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
    mutationFn: ({ nodeId, command }: { nodeId: string; command: ExecuteCommandRequest }) =>
      api.executeCommand(nodeId, command),
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
  node_id?: string;
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
