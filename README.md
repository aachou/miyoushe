# miyoushe

爬取米游社（miyoushe.com）用户主页中**所有帖子的图片**。

## 下载

```bash
cargo install miyoushe
```

## 用法

```bash
# 直接给用户 ID
miyoushe 76438443

# 或给完整主页链接（从 URL 中解析 id）
miyoushe "https://www.miyoushe.com/sr/accountCenter/postList?id=76438443"
```

图片会保存在系统图片目录。

### 参数

| 参数 | 默认 | 说明 |
| --- | --- | --- |
| `target` | — | 用户 ID 或主页链接 |
| `-o, --output <DIR>` | 系统图片目录 | 输出根目录，其下创建昵称文件夹 |
| `--min-size <SIZE>` | `0` | 最小图片大小，如 `500KB` / `2MB` / `512000`；小于阈值的图片下载完成后删除 |
| `--concurrency <N>` | `4` | 并发下载数 |
| `--overwrite` | 关 | 已存在的文件重新下载（默认跳过已达标文件，可断点续爬） |
| `--limit <N>` | `0` | 只处理前 N 张图片（0 = 全部），便于试跑 |
