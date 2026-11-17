// SPDX-License-Identifier: GPL-2.0-only
/* Linux's sorts, with the interfaces of Linux
 * cc9de5d4b5ab6459f5774946aa3195ad51236fa8 lib/sort.c and lib/list_sort.c: an
 * in-place, unstable heapsort of arrays, with or without a private pointer
 * for its functions, and a stable merge sort of lists. */
#include <linux/list.h>
#include <linux/list_sort.h>
#include <linux/sort.h>
#include <linux/string.h>

static void swap_bytes(void *a, void *b, int size)
{
	u8 *x = a, *y = b;

	while (size--) {
		u8 byte = *x;

		*x++ = *y;
		*y++ = byte;
	}
}

/* One sort's functions: sort_r()'s, which take its private pointer, or
 * sort()'s, which do not. */
struct order {
	cmp_r_func_t compare_r;
	swap_r_func_t swap_r;
	cmp_func_t compare;
	swap_func_t swap;
	const void *priv;
};

static int compare_elements(const struct order *order, const void *a, const void *b)
{
	return order->compare_r ? order->compare_r(a, b, order->priv) : order->compare(a, b);
}

static void swap_elements(const struct order *order, void *a, void *b, size_t size)
{
	if (order->swap_r)
		order->swap_r(a, b, (int)size, order->priv);
	else if (order->swap)
		order->swap(a, b, (int)size);
	else
		swap_bytes(a, b, (int)size);
}

/* Lets the element at root sink below its larger children in a heap of count
 * elements. */
static void sift_down(const struct order *order, char *base, size_t root, size_t count,
		      size_t size)
{
	for (size_t child; (child = 2 * root + 1) < count; root = child) {
		if (child + 1 < count &&
		    compare_elements(order, base + child * size, base + (child + 1) * size) < 0)
			child++;
		if (compare_elements(order, base + root * size, base + child * size) >= 0)
			break;
		swap_elements(order, base + root * size, base + child * size, size);
	}
}

static void heapsort(const struct order *order, void *base, size_t num, size_t size)
{
	for (size_t root = num / 2; root--;)
		sift_down(order, base, root, num, size);
	for (size_t end = num; end > 1; end--) {
		swap_elements(order, base, (char *)base + (end - 1) * size, size);
		sift_down(order, base, 0, end - 1, size);
	}
}

void sort(void *base, size_t num, size_t size, cmp_func_t cmp_func, swap_func_t swap_func)
{
	heapsort(&(struct order){.compare = cmp_func, .swap = swap_func}, base, num, size);
}

void sort_r(void *base, size_t num, size_t size, cmp_r_func_t cmp_func, swap_r_func_t swap_func,
	    const void *priv)
{
	heapsort(&(struct order){.compare_r = cmp_func, .swap_r = swap_func, .priv = priv}, base,
		 num, size);
}

/* Merges two sorted null-terminated chains linked by next, earlier a first. */
static struct list_head *merge(void *priv, list_cmp_func_t cmp, struct list_head *a,
			       struct list_head *b)
{
	struct list_head *head = NULL, **tail = &head;

	while (a && b) {
		struct list_head **least = cmp(priv, a, b) <= 0 ? &a : &b;

		*tail = *least;
		tail = &(*least)->next;
		*least = (*least)->next;
	}
	*tail = a ? a : b;
	return head;
}

/* Sorted chains of doubling length, as bottom-up merge sort keeps them. */
void list_sort(void *priv, struct list_head *head, list_cmp_func_t cmp)
{
	struct list_head *runs[BITS_PER_LONG] = {}, *list = head->next;

	if (list == head || list->next == head)
		return;
	head->prev->next = NULL;
	while (list) {
		struct list_head *run = list;
		unsigned int rank = 0;

		list = list->next;
		run->next = NULL;
		for (; runs[rank]; rank++) {
			run = merge(priv, cmp, runs[rank], run);
			runs[rank] = NULL;
		}
		runs[rank] = run;
	}
	for (unsigned int rank = 0; rank < ARRAY_SIZE(runs); rank++)
		if (runs[rank])
			list = merge(priv, cmp, runs[rank], list);

	/* Back to a circular list, with its prev links. */
	struct list_head *prev = head;

	for (head->next = list; list; prev = list, list = list->next)
		list->prev = prev;
	prev->next = head;
	head->prev = prev;
}

#ifdef STELA_LINUX_SORT_TEST
#include <linux/minmax.h>
#include <stela/test.h>

struct item {
	struct list_head node;
	int key;
	int order;
};

static int compare_ints(const void *a, const void *b)
{
	return *(const int *)a - *(const int *)b;
}

/* Descending when the private pointer says so. */
static int compare_ints_r(const void *a, const void *b, const void *priv)
{
	int order = *(const int *)a - *(const int *)b;

	return *(const bool *)priv ? -order : order;
}

static unsigned int swaps;

static void swap_ints_r(void *a, void *b, int size, const void *priv)
{
	assert(size == sizeof(int) && priv);
	swap(*(int *)a, *(int *)b);
	swaps++;
}

static int compare_items(void *priv, const struct list_head *a, const struct list_head *b)
{
	(void)priv;
	return container_of(a, struct item, node)->key - container_of(b, struct item, node)->key;
}

int main(void)
{
	int values[] = {5, -1, 9, 3, 3, 0, 7, -8, 2};

	sort(values, ARRAY_SIZE(values), sizeof(*values), compare_ints, NULL);
	for (size_t i = 1; i < ARRAY_SIZE(values); i++)
		assert(values[i - 1] <= values[i]);
	sort(values, 0, sizeof(*values), compare_ints, NULL);

	bool descending = true;

	sort_r(values, ARRAY_SIZE(values), sizeof(*values), compare_ints_r, NULL, &descending);
	for (size_t i = 1; i < ARRAY_SIZE(values); i++)
		assert(values[i - 1] >= values[i]);
	descending = false;
	sort_r(values, ARRAY_SIZE(values), sizeof(*values), compare_ints_r, swap_ints_r,
	       &descending);
	for (size_t i = 1; i < ARRAY_SIZE(values); i++)
		assert(values[i - 1] <= values[i]);
	assert(swaps);

	struct item items[11];
	LIST_HEAD(list);

	list_sort(NULL, &list, compare_items);
	assert(list_empty(&list));
	for (int i = 0; i < 11; i++) {
		items[i] = (struct item){.key = (i * 7) % 3, .order = i};
		list_add_tail(&items[i].node, &list);
	}
	list_sort(NULL, &list, compare_items);
	unsigned int count = 0;
	const struct item *previous = NULL, *item;

	list_for_each_entry(item, &list, node) {
		assert(!previous || previous->key < item->key ||
		       (previous->key == item->key && previous->order < item->order));
		assert(!previous || item->node.prev == &previous->node);
		previous = item;
		count++;
	}
	assert(count == 11 && list.prev == &previous->node);
	printf("Linux's sorts passed\n");
	return 0;
}
#endif
