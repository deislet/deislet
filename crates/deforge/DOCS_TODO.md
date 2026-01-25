# 文档待更新列表

本文档列出需要更新的文档文件，以便与当前实现保持一致。

## ✅ 已完成

- [x] README.md - 已更新至 Edge Canon 和当前架构
- [x] BASIC_SERVICES.md - 新建，说明基础服务
- [x] crates/deforge-core/ERROR_SYSTEM.md - 新建，错误分类系统

## 🔄 待更新

### 高优先级

- [ ] **ARCHITECTURE.md** - 需要完全重写
  - 当前引用的是 @denictl (TypeScript 实现)
  - 需要更新为 Rust 实现的架构
  - 需要添加平台能力系统、编译器流程、适配器系统
  - 参考: 当前 src/ 和 crates/ 结构

- [ ] **QUICKSTART.md** - 需要更新
  - 当前引用的是 denictl 命令
  - 需要更新为 deforge 命令
  - 需要更新配置文件格式（.config.json）
  - 需要更新示例代码以反映基础服务

### 中优先级

- [ ] **CONTRIBUTING.md** - 检查是否需要更新
  - 确认贡献流程是否与当前实现匹配
  - 确认测试指南是否正确
  - 确认代码规范是否正确

- [ ] **cli-compiler-requirements.md** - 检查相关性
  - 确认是否仍然需要这个文件
  - 或者合并到其他文档

### 低优先级

- [ ] crates/ADAPTER_SPECS.md - 如果存在的话需要检查
- [ ] examples/hello-world/README.md - 确保示例代码更新

## 📝 新增文档建议

建议新增以下文档：

- [ ] **PLATFORM_CAPABILITIES.md** - 详细说明平台能力验证系统
- [ ] **COMPILER_WORKFLOW.md** - 编译流程详解
- [ ] **ADAPTER_GUIDE.md** - 如何为新平台创建适配器
- [ ] **MIGRATION_GUIDE.md** - 从旧版本迁移指南

## 🔗 相关外部文档

需要确保以下外部文档链接正确：

- [x] Edge Canon 基础规范
- [x] Edge Canon 扩展规范
- [x] Edge Canon 平台支持矩阵
- [ ] Cloudflare Workers 文档
- [ ] Deno Deploy 文档
- [ ] Tencent EdgeOne 文档

## 更新原则

更新文档时应遵循以下原则：

1. **准确性**: 确保与代码实现完全一致
2. **完整性**: 涵盖所有关键功能和概念
3. **清晰性**: 使用示例和图表说明复杂概念
4. **一致性**: 术语和命名与 Edge Canon 规范一致
5. **时效性**: 标注文档版本和最后更新日期
