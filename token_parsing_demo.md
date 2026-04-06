# Token Parsing Implementation Demo

## 已实现的功能

### 1. **TokenInfo和CacheTokenInfo数据结构**
- 在`src/services/opencode/types.rs`中添加了完整的token信息结构
- 支持完整的token信息解析：input, output, reasoning, cache read/write
- 所有字段都有默认值，支持部分字段缺失的情况

### 2. **SSE step-finish事件处理增强**
- 在`src/services/opencode/client.rs`中增强了step-finish事件处理
- 解析token JSON并显示详细的token使用信息
- 提供优雅的错误处理：当解析失败时回退显示原始JSON

### 3. **SseResult结构扩展**
- 添加了完整的token计数字段：
  - `input_tokens: Option<u64>`
  - `output_tokens: Option<u64>`
  - `reasoning_tokens: Option<u64>`
  - `cache_read_tokens: Option<u64>`
  - `cache_write_tokens: Option<u64>`
  - `total_cost: Option<f64>`

### 4. **日志输出改进**
- 当token信息可用时，显示详细的token使用统计
- 日志级别：`tracing::info!`（之前是`debug!`）
- 格式：显示每个token类型的详细计数和总计

## 示例日志输出

当step-finish事件包含token信息时：

```
Step finished with token details:
  Reason: stop
  Cost: $0.123
  Tokens:
    Input: 1500
    Output: 250
    Reasoning: 300
    Cache:
      Read: 100
      Write: 50
    Total: 2050
```

当token信息不可用或解析失败时：

```
Step finished (no token information)
```

或

```
Step finished (failed to parse tokens: ...)
```

## 测试覆盖

### 新增的单元测试
1. **test_token_info_parsing** - 测试完整的token信息解析
2. **test_token_info_with_missing_fields** - 测试部分字段缺失的情况
3. **test_cache_token_info_default** - 测试CacheTokenInfo的默认值

## 向后兼容性

- 所有现有功能保持不变
- 当token信息不可用时，不影响正常流程
- 所有现有测试仍然通过（115 → 118个测试）

## 如何使用

### 获取input token数量
1. 通过SSE事件流监听`message.part.updated`事件
2. 当`part.type`为`"step-finish"`时，检查`part.tokens`字段
3. 解析JSON获取`tokens.input`字段

### 在代码中访问
```rust
// 在SSE处理完成后
let sse_result: SseResult = ...;
if let Some(input_tokens) = sse_result.input_tokens {
    println!("Input tokens used: {}", input_tokens);
}
```

## 技术实现细节

### 数据结构
```rust
pub struct TokenInfo {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache: CacheTokenInfo,
}

pub struct CacheTokenInfo {
    pub read: u64,
    pub write: u64,
}
```

### 错误处理
- 使用`serde_json::from_value`解析JSON
- 失败时回退到原始JSON显示
- 所有字段都有`#[serde(default)]`属性，支持缺失字段

### 日志级别
- 详细token信息：`info!`级别
- 解析失败或缺少token信息：`debug!`级别