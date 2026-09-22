# 拷贝漫画下载器

## 介绍
基于 reqwest 的命令行漫画下载器（CLI 版）。

## 功能
- 按关键词搜索漫画
- 选择话数区间下载
- 检查本地漫画更新并下载新章节

## 构建
在根目录下运行：

```
cargo build --release
```

生成的 exe 位于 `target/release/` 下。

## 注意
- 站点地址已硬编码在代码中，无需 config.toml。
- 仅支持 windows8/10/11
