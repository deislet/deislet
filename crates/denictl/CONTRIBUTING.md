# Contributing to DeniCTL

感谢您对 DeniCTL 的贡献兴趣!本文档提供了项目开发指南。

## 开发环境设置

### 前置要求

- Node.js >= 18.0.0
- pnpm >= 8.0.0

### 安装依赖

```bash
git clone https://github.com/deislet/denictl.git
cd denictl
pnpm install
```

### 构建项目

```bash
# 构建所有包
pnpm build

# 监听模式 (开发时使用)
pnpm dev
```

### 运行测试

```bash
# 运行所有测试
pnpm test

# 运行 lint
pnpm lint
```

## 项目结构

```
denictl/
├── packages/
│   ├── types/       # 类型定义
│   ├── cli/         # CLI 工具
│   ├── compiler/    # 编译器
│   └── adapters/    # 平台适配器
├── cli-compiler-requirements.md
└── README.md
```

## 开发工作流

### 1. 添加新功能

1. 创建功能分支:
   ```bash
   git checkout -b feature/my-feature
   ```

2. 实现功能并添加测试

3. 运行测试和 lint:
   ```bash
   pnpm test
   pnpm lint
   ```

4. 提交更改:
   ```bash
   git commit -m "feat: add my feature"
   ```

5. 推送并创建 Pull Request

### 2. 修复 Bug

1. 创建 bugfix 分支:
   ```bash
   git checkout -b fix/bug-description
   ```

2. 修复 bug 并添加回归测试

3. 提交:
   ```bash
   git commit -m "fix: description of bug fix"
   ```

### 3. 添加新平台支持

参考 `packages/adapters/src/cloudflare/` 的实现模式:

1. 在 `packages/adapters/src/` 下创建新目录
2. 实现 KV、Database 等适配器
3. 实现 Context Builder
4. 在 Transformer 中添加入口点生成
5. 添加集成测试

## 代码规范

### TypeScript

- 使用严格模式 (`strict: true`)
- 所有公共 API 必须有完整的类型定义
- 避免使用 `any`,使用 `unknown` 代替
- 使用 ESM 模块系统

### 命名规范

- 文件名: kebab-case (例如: `kv-adapter.ts`)
- 类名: PascalCase (例如: `Compiler`)
- 函数名: camelCase (例如: `createKVAdapter`)
- 常量: UPPER_SNAKE_CASE (例如: `DEFAULT_PORT`)

### 注释

- 所有公共 API 必须有 JSDoc 注释
- 复杂逻辑添加行内注释
- 使用 TODO 标记待办事项

示例:
```typescript
/**
 * Create KV adapter for Cloudflare Workers KV
 * @param namespace - Cloudflare KV namespace binding
 * @returns Unified KVStore interface
 */
export function createKVAdapter(namespace: any): KVStore {
  // Implementation
}
```

## 提交规范

遵循 [Conventional Commits](https://www.conventionalcommits.org/):

- `feat:` 新功能
- `fix:` Bug 修复
- `docs:` 文档更新
- `style:` 代码格式调整
- `refactor:` 重构
- `test:` 测试相关
- `chore:` 构建/工具相关

示例:
```
feat(compiler): add support for middleware
fix(cli): resolve init command path issue
docs: update architecture documentation
```

## 测试指南

### 单元测试

使用 Vitest:

```typescript
import { describe, it, expect } from 'vitest';
import { Parser } from './parser';

describe('Parser', () => {
  it('should extract exports correctly', () => {
    const parser = new Parser();
    const result = parser.parse(code, 'test.ts');
    expect(result.exports.hasDefault).toBe(true);
  });
});
```

### 集成测试

测试完整的编译流程:

```typescript
describe('Compiler Integration', () => {
  it('should compile project successfully', async () => {
    const compiler = new Compiler();
    const result = await compiler.compile({
      projectDir: './fixtures/test-project',
      vendors: ['cloudflare']
    });
    expect(result.success).toBe(true);
  });
});
```

## Pull Request 流程

1. Fork 项目
2. 创建功能分支
3. 提交更改
4. 推送到 fork
5. 创建 Pull Request

### PR 要求

- 清晰的描述
- 相关的 issue 链接
- 测试覆盖
- 通过 CI 检查
- 代码审查通过

## 问题反馈

使用 GitHub Issues 报告问题,请包含:

- 问题描述
- 重现步骤
- 期望行为
- 实际行为
- 环境信息 (Node 版本, OS 等)

## 许可证

通过贡献代码,您同意您的贡献将在 MIT 许可证下发布。
